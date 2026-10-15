//! `PostgreSQL` behind the seam: deadpool's pool, rustls on ring, and the
//! seam's values as the Rust types `tokio-postgres` binds.

use deadpool_postgres::{GenericClient, Manager, ManagerConfig, Pool, RecyclingMethod};
use time::OffsetDateTime;
use tokio_postgres::types::{ToSql, Type};

use crate::postgres::{CONNECT_TIMEOUT, POOL_SIZE, parse_url, tls};
use crate::types::StoreError;

use super::migrate;
use super::seam::{Cell, Dialect, Kind, Row, Stmt, Value, rfc3339};

/// The `PostgreSQL` database.
#[derive(Debug)]
pub(crate) struct PgDb {
    pool: Pool,
}

impl PgDb {
    /// Connects to `url` and migrates the schema up to unified step `upto`.
    pub(crate) async fn connect(url: &str, upto: usize) -> Result<Self, StoreError> {
        let mut config = parse_url(url)?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(CONNECT_TIMEOUT);
        }
        let manager = Manager::from_config(
            config,
            tls()?,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(POOL_SIZE)
            .build()
            .map_err(|error| StoreError::Connect(error.to_string()))?;
        migrate::postgres(&mut pool.get().await?, upto).await?;
        Ok(Self { pool })
    }

    pub(crate) async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<u64, StoreError> {
        let client = self.pool.get().await?;
        execute(&client, sql, &params).await
    }

    pub(crate) async fn query(
        &self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Vec<Row>, StoreError> {
        let client = self.pool.get().await?;
        let statement = prepare(&client, sql, &params).await?;
        let bound = bind(&params);
        let rows = client.query(&statement, &refs(&bound)).await?;
        rows.iter().map(read).collect()
    }

    pub(crate) async fn atomically(&self, statements: Vec<Stmt>) -> Result<Vec<u64>, StoreError> {
        let mut client = self.pool.get().await?;
        let transaction = client.transaction().await?;
        let mut changed = Vec::with_capacity(statements.len());
        for statement in &statements {
            changed.push(execute(&transaction, &statement.sql, &statement.params).await?);
        }
        transaction.commit().await?;
        Ok(changed)
    }
}

/// A client or a transaction runs a statement alike.
async fn execute(
    client: &impl GenericClient,
    sql: &str,
    params: &[Value],
) -> Result<u64, StoreError> {
    let statement = prepare(client, sql, params).await?;
    let bound = bind(params);
    Ok(client.execute(&statement, &refs(&bound)).await?)
}

/// The statement with its parameter types given from the values, so that
/// the server never has to guess one (`$1 IS NULL OR kind = $1` is not
/// enough for it): an integer is 8 bytes, a time a `TIMESTAMPTZ`.
async fn prepare(
    client: &impl GenericClient,
    sql: &str,
    params: &[Value],
) -> Result<tokio_postgres::Statement, StoreError> {
    let types: Vec<Type> = params
        .iter()
        .map(|value| match value {
            Value::Int(_) | Value::Null(Kind::Int) => Type::INT8,
            Value::Text(_) | Value::Null(Kind::Text) => Type::TEXT,
            Value::Time(..) | Value::Instant(_) | Value::Null(Kind::Time) => Type::TIMESTAMPTZ,
        })
        .collect();
    Ok(client
        .prepare_typed_cached(&Dialect::Postgres.sql(sql), &types)
        .await?)
}

/// A parameter list as values of the Rust types the statement was typed with.
fn bind(values: &[Value]) -> Vec<Box<dyn ToSql + Sync + Send>> {
    values
        .iter()
        .map(|value| -> Box<dyn ToSql + Sync + Send> {
            match value {
                Value::Null(Kind::Int) => Box::new(None::<i64>),
                Value::Null(Kind::Text) => Box::new(None::<String>),
                Value::Null(Kind::Time) => Box::new(None::<OffsetDateTime>),
                Value::Int(n) => Box::new(*n),
                Value::Text(text) => Box::new(text.clone()),
                Value::Time(at, _) | Value::Instant(at) => Box::new(*at),
            }
        })
        .collect()
}

fn refs(bound: &[Box<dyn ToSql + Sync + Send>]) -> Vec<&(dyn ToSql + Sync)> {
    bound.iter().map(|b| &**b as &(dyn ToSql + Sync)).collect()
}

/// A row as seam cells: times as RFC 3339 text, booleans as 0 or 1.
fn read(row: &tokio_postgres::Row) -> Result<Row, StoreError> {
    let mut cells = Vec::with_capacity(row.len());
    for (i, column) in row.columns().iter().enumerate() {
        let ty = column.type_();
        cells.push(if *ty == Type::INT8 {
            row.try_get::<_, Option<i64>>(i)?
                .map_or(Cell::Null, Cell::Int)
        } else if *ty == Type::INT4 {
            row.try_get::<_, Option<i32>>(i)?
                .map_or(Cell::Null, |n| Cell::Int(i64::from(n)))
        } else if *ty == Type::BOOL {
            row.try_get::<_, Option<bool>>(i)?
                .map_or(Cell::Null, |b| Cell::Int(i64::from(b)))
        } else if *ty == Type::TEXT {
            row.try_get::<_, Option<String>>(i)?
                .map_or(Cell::Null, Cell::Text)
        } else if *ty == Type::TIMESTAMPTZ {
            row.try_get::<_, Option<OffsetDateTime>>(i)?
                .map_or(Cell::Null, |at| Cell::Text(rfc3339(at)))
        } else {
            return Err(StoreError::Schema(format!(
                "column {} is a {ty}, which the store does not read",
                column.name()
            )));
        });
    }
    Ok(Row(cells))
}
