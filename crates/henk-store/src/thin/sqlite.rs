//! `SQLite` behind the seam: a small pool of connections, each used on the
//! blocking thread pool so a query never holds up the async runtime.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rusqlite::types::{ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, TransactionBehavior};
use time::UtcOffset;

use crate::types::StoreError;

use super::migrate;
use super::seam::{Cell, Row, Stmt, Value};

/// Connections of a file database. WAL lets readers run beside the writer;
/// the writer waits for its turn (see [`BUSY_TIMEOUT`]).
const POOL_SIZE: usize = 4;

/// How long a writer waits for the write lock before "database is locked".
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The `SQLite` database.
#[derive(Debug)]
pub(crate) struct SqliteDb {
    pool: Arc<Pool>,
}

/// Idle connections, and who waits for one.
#[derive(Debug)]
struct Pool {
    idle: Mutex<Vec<Connection>>,
    returned: Condvar,
}

/// A connection on loan; it goes back when this drops, also in a panic.
struct Loan {
    pool: Arc<Pool>,
    connection: Option<Connection>,
}

impl Drop for Loan {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            if let Ok(mut idle) = self.pool.idle.lock() {
                idle.push(connection);
            }
            self.pool.returned.notify_one();
        }
    }
}

impl Pool {
    fn take(self: &Arc<Self>) -> Result<Loan, StoreError> {
        let mut idle = self.idle.lock().map_err(|_| StoreError::Poisoned)?;
        loop {
            if let Some(connection) = idle.pop() {
                return Ok(Loan {
                    pool: Arc::clone(self),
                    connection: Some(connection),
                });
            }
            idle = self.returned.wait(idle).map_err(|_| StoreError::Poisoned)?;
        }
    }
}

fn setup(connection: &Connection) -> Result<(), StoreError> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

impl SqliteDb {
    /// Opens the database at `path` (or in memory when `None`) and migrates
    /// it up to unified step `upto`.
    pub(crate) fn open(path: Option<&Path>, upto: usize) -> Result<Self, StoreError> {
        let open = || match path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        };
        let mut first = open()?;
        setup(&first)?;
        migrate::sqlite(&mut first, upto)?;
        // An in-memory database is one connection's own.
        let size = if path.is_some() { POOL_SIZE } else { 1 };
        let mut idle = vec![first];
        for _ in 1..size {
            let connection = open()?;
            setup(&connection)?;
            idle.push(connection);
        }
        Ok(Self {
            pool: Arc::new(Pool {
                idle: Mutex::new(idle),
                returned: Condvar::new(),
            }),
        })
    }

    /// Runs `work` on a connection, on a blocking thread.
    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || {
            let mut loan = pool.take()?;
            let connection = loan.connection.as_mut().ok_or(StoreError::Poisoned)?;
            work(connection)
        })
        .await
        .map_err(|_| StoreError::Poisoned)?
    }

    pub(crate) async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<u64, StoreError> {
        let sql = sql.to_owned();
        self.run(move |c| execute(c, &sql, &params)).await
    }

    pub(crate) async fn query(
        &self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Vec<Row>, StoreError> {
        let sql = sql.to_owned();
        self.run(move |c| query(c, &sql, &params)).await
    }

    pub(crate) async fn atomically(&self, statements: Vec<Stmt>) -> Result<Vec<u64>, StoreError> {
        self.run(move |c| {
            // IMMEDIATE takes the write lock now, so busy_timeout applies;
            // a deferred transaction that turns into a write can fail at once.
            let transaction = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut changed = Vec::with_capacity(statements.len());
            for statement in &statements {
                changed.push(execute(&transaction, &statement.sql, &statement.params)?);
            }
            transaction.commit()?;
            Ok(changed)
        })
        .await
    }
}

fn execute(connection: &Connection, sql: &str, params: &[Value]) -> Result<u64, StoreError> {
    let changed = connection
        .prepare_cached(sql)?
        .execute(rusqlite::params_from_iter(params))?;
    Ok(u64::try_from(changed).unwrap_or(u64::MAX))
}

fn query(connection: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Row>, StoreError> {
    let mut statement = connection.prepare_cached(sql)?;
    let columns = statement.column_count();
    let mut rows = statement.query(rusqlite::params_from_iter(params))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns);
        for i in 0..columns {
            values.push(match row.get_ref(i)? {
                ValueRef::Null => Cell::Null,
                ValueRef::Integer(n) => Cell::Int(n),
                ValueRef::Text(text) => Cell::Text(
                    std::str::from_utf8(text)
                        .map_err(|_| StoreError::Schema(format!("column {i} is not UTF-8")))?
                        .to_owned(),
                ),
                other => {
                    return Err(StoreError::Schema(format!(
                        "column {i}: a {:?} is not a value the store reads",
                        other.data_type()
                    )));
                }
            });
        }
        out.push(Row(values));
    }
    Ok(out)
}

impl ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        use rusqlite::types::Value as Sql;
        Ok(ToSqlOutput::Owned(match self {
            Self::Null(_) => Sql::Null,
            Self::Int(n) => Sql::Integer(*n),
            Self::Text(text) | Self::Time(_, text) => Sql::Text(text.clone()),
            Self::Instant(at) => {
                let at = at.to_offset(UtcOffset::UTC);
                Sql::Text(format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{:09}",
                    at.year(),
                    u8::from(at.month()),
                    at.day(),
                    at.hour(),
                    at.minute(),
                    at.second(),
                    at.nanosecond()
                ))
            }
        }))
    }
}
