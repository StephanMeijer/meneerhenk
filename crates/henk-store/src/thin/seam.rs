//! The seam of the thin store: what a query says, in one dialect-neutral
//! form, and the few places where the two databases differ.
//!
//! A query is `SQL text with ?1, ?2 placeholders` plus a list of [`Value`]s.
//! A [`Row`] is a list of [`Value`]s. [`Dialect`] writes the fragments that
//! cannot be common. [`Db`] runs it all on SQLite or `PostgreSQL`.

use std::borrow::Cow;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::types::StoreError;

use super::pg::PgDb;
use super::sqlite::SqliteDb;

/// The two databases, for the places where their SQL cannot be the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    /// A stored time column as something that compares as a time. `PostgreSQL`
    /// stores `TIMESTAMPTZ`. `SQLite` stores RFC 3339 text, whose order is not
    /// the time's (".4Z" sorts before "Z", #216), so the column becomes a
    /// fixed-width UTC text: `2026-10-07T12:00:00` and nine digits of
    /// nanoseconds, as [`Value::Instant`] binds. `migrations/unified/
    /// 013_run_instant.sqlite.sql` indexes exactly this expression.
    pub(crate) fn instant(self, column: &str) -> String {
        match self {
            Self::Postgres => column.to_owned(),
            Self::Sqlite => format!(
                "substr({column}, 1, 19) || substr(CASE WHEN substr({column}, 20, 1) = '.' THEN substr({column}, 21, length({column}) - 21) ELSE '' END || '000000000', 1, 9)"
            ),
        }
    }

    /// The UTC day of a stored time column, `YYYY-MM-DD`.
    pub(crate) fn day(self, column: &str) -> String {
        match self {
            Self::Postgres => format!("to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD')"),
            Self::Sqlite => format!("substr({column}, 1, 10)"),
        }
    }

    /// The column that orders `event_outcomes` and `requests` rows as they
    /// were written: `PostgreSQL` has an identity column, `SQLite` its rowid.
    pub(crate) fn written(self) -> &'static str {
        match self {
            Self::Postgres => "id",
            Self::Sqlite => "rowid",
        }
    }

    /// `sql` written with `?1` placeholders, as the database takes it.
    pub(crate) fn sql(self, sql: &str) -> Cow<'_, str> {
        match self {
            Self::Sqlite => Cow::Borrowed(sql),
            Self::Postgres => Cow::Owned(dollars(sql)),
        }
    }
}

/// `?12` as `$12`, outside quoted text.
fn dollars(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut quoted = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = !quoted;
                out.push(c);
            }
            '?' if !quoted && chars.peek().is_some_and(char::is_ascii_digit) => out.push('$'),
            _ => out.push(c),
        }
    }
    out
}

/// `?first, ?first+1, ..` for an `IN (..)` list of `count` values.
pub(crate) fn in_list(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The types a parameter or a cell can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Text,
    Int,
    Time,
}

/// A value going into a statement.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// No value, of the type the parameter has: `PostgreSQL` has to be told.
    Null(Kind),
    Int(i64),
    Text(String),
    /// A time to store, as RFC 3339 text: kept as given on `SQLite`, a
    /// `TIMESTAMPTZ` on `PostgreSQL`.
    Time(OffsetDateTime, String),
    /// A time to compare stored times with: the fixed-width text of
    /// [`Dialect::instant`] on `SQLite`, a `TIMESTAMPTZ` on `PostgreSQL`.
    Instant(OffsetDateTime),
}

impl Value {
    /// A time to store: `text` is an RFC 3339 time, or empty for now.
    pub(crate) fn time(column: &'static str, text: &str) -> Result<Self, StoreError> {
        if text.is_empty() {
            return Ok(Self::now());
        }
        let at = parse(column, text)?;
        Ok(Self::Time(at, text.to_owned()))
    }

    pub(crate) fn now() -> Self {
        let at = OffsetDateTime::now_utc();
        Self::Time(at, rfc3339(at))
    }

    pub(crate) fn time_at(at: OffsetDateTime) -> Self {
        Self::Time(at, rfc3339(at))
    }

    /// A time to compare with: an RFC 3339 time.
    pub(crate) fn instant(column: &'static str, text: &str) -> Result<Self, StoreError> {
        Ok(Self::Instant(parse(column, text)?))
    }

    pub(crate) fn instant_at(at: OffsetDateTime) -> Self {
        Self::Instant(at)
    }

    /// A time that may be missing.
    pub(crate) fn no_time() -> Self {
        Self::Null(Kind::Time)
    }

    /// [`Self::instant`] of a time that may be missing.
    pub(crate) fn instant_or_none(
        column: &'static str,
        text: Option<&str>,
    ) -> Result<Self, StoreError> {
        text.map_or_else(|| Ok(Self::no_time()), |text| Self::instant(column, text))
    }

    /// [`Self::time`] of a time that may be missing (not "now": missing).
    pub(crate) fn time_or_none(
        column: &'static str,
        text: Option<&str>,
    ) -> Result<Self, StoreError> {
        text.map_or_else(|| Ok(Self::no_time()), |text| Self::time(column, text))
    }
}

fn parse(column: &'static str, text: &str) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::parse(text, &Rfc3339).map_err(|_| StoreError::Corrupt {
        column,
        value: text.to_owned(),
    })
}

/// An RFC 3339 time as text.
pub(crate) fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// What can be a statement parameter.
pub(crate) trait ToValue {
    fn to_value(&self) -> Value;
}

/// What can be a parameter that is sometimes missing: it knows its type.
pub(crate) trait Nullable: ToValue {
    fn null() -> Value;
}

impl ToValue for str {
    fn to_value(&self) -> Value {
        Value::Text(self.to_owned())
    }
}

impl Nullable for str {
    fn null() -> Value {
        Value::Null(Kind::Text)
    }
}

impl ToValue for String {
    fn to_value(&self) -> Value {
        self.as_str().to_value()
    }
}

impl Nullable for String {
    fn null() -> Value {
        Value::Null(Kind::Text)
    }
}

impl ToValue for i64 {
    fn to_value(&self) -> Value {
        Value::Int(*self)
    }
}

impl Nullable for i64 {
    fn null() -> Value {
        Value::Null(Kind::Int)
    }
}

impl ToValue for u32 {
    fn to_value(&self) -> Value {
        Value::Int(i64::from(*self))
    }
}

/// A value is its own parameter; a missing one is made with
/// [`Value::no_time`].
impl ToValue for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}

impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}

impl<T: Nullable + ?Sized> Nullable for &T {
    fn null() -> Value {
        T::null()
    }
}

impl<T: Nullable> ToValue for Option<T> {
    fn to_value(&self) -> Value {
        self.as_ref().map_or_else(T::null, ToValue::to_value)
    }
}

/// A parameter list: `params![a, b, c]`.
macro_rules! params {
    ($($value:expr),* $(,)?) => {
        vec![$($crate::thin::seam::ToValue::to_value(&$value)),*]
    };
}
pub(crate) use params;

/// One cell of a result row. `PostgreSQL` times arrive as RFC 3339 text, as
/// `SQLite`'s are stored, and its booleans as 0 or 1.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Cell {
    Null,
    Int(i64),
    Text(String),
}

/// One result row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row(pub(crate) Vec<Cell>);

impl Row {
    fn get(&self, i: usize) -> Result<&Cell, StoreError> {
        self.0
            .get(i)
            .ok_or_else(|| StoreError::Schema(format!("the query returned no column {i}")))
    }

    fn mismatch(i: usize, wanted: &str, got: &Cell) -> StoreError {
        StoreError::Schema(format!("column {i}: wanted {wanted}, got {got:?}"))
    }

    pub(crate) fn opt_text(&self, i: usize) -> Result<Option<String>, StoreError> {
        match self.get(i)? {
            Cell::Null => Ok(None),
            Cell::Text(text) => Ok(Some(text.clone())),
            got @ Cell::Int(_) => Err(Self::mismatch(i, "text or null", got)),
        }
    }

    pub(crate) fn text(&self, i: usize) -> Result<String, StoreError> {
        self.opt_text(i)?
            .ok_or_else(|| Self::mismatch(i, "text", &Cell::Null))
    }

    pub(crate) fn opt_int(&self, i: usize) -> Result<Option<i64>, StoreError> {
        match self.get(i)? {
            Cell::Null => Ok(None),
            Cell::Int(n) => Ok(Some(*n)),
            got @ Cell::Text(_) => Err(Self::mismatch(i, "an integer or null", got)),
        }
    }

    pub(crate) fn int(&self, i: usize) -> Result<i64, StoreError> {
        self.opt_int(i)?
            .ok_or_else(|| Self::mismatch(i, "an integer", &Cell::Null))
    }

    pub(crate) fn bool(&self, i: usize) -> Result<bool, StoreError> {
        Ok(self.int(i)? != 0)
    }
}

/// A statement for [`Db::atomically`].
pub(crate) struct Stmt {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

/// A database, either of the two.
#[derive(Debug)]
pub(crate) enum Db {
    Sqlite(SqliteDb),
    Postgres(PgDb),
}

impl Db {
    pub(crate) fn dialect(&self) -> Dialect {
        match self {
            Self::Sqlite(_) => Dialect::Sqlite,
            Self::Postgres(_) => Dialect::Postgres,
        }
    }

    /// Runs a statement; the number of rows it changed.
    pub(crate) async fn execute(&self, sql: &str, params: Vec<Value>) -> Result<u64, StoreError> {
        match self {
            Self::Sqlite(db) => db.execute(sql, params).await,
            Self::Postgres(db) => db.execute(sql, params).await,
        }
    }

    /// Runs a query; its rows.
    pub(crate) async fn query(
        &self,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<Vec<Row>, StoreError> {
        match self {
            Self::Sqlite(db) => db.query(sql, params).await,
            Self::Postgres(db) => db.query(sql, params).await,
        }
    }

    /// Runs the statements in one transaction: all of them or none. The
    /// rows each changed.
    pub(crate) async fn atomically(&self, statements: Vec<Stmt>) -> Result<Vec<u64>, StoreError> {
        match self {
            Self::Sqlite(db) => db.atomically(statements).await,
            Self::Postgres(db) => db.atomically(statements).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_become_dollars_outside_quotes() {
        assert_eq!(
            dollars("SELECT '?1', ?1, ?12 FROM t WHERE a = ?2 AND b = 'x?'"),
            "SELECT '?1', $1, $12 FROM t WHERE a = $2 AND b = 'x?'"
        );
    }

    #[test]
    fn an_in_list_counts_from_where_it_starts() {
        assert_eq!(in_list(2, 3), "?2, ?3, ?4");
    }
}
