//! The seam: what the two databases do differently, named once, so no method
//! of the store has to ask which database it is on.
//!
//! Placeholders (`?` or `$1`), quoting, upserts and `RETURNING` are the query
//! builder's business and need no help. Time is the one thing left, because
//! the two databases keep it differently:
//!
//! - `PostgreSQL` has `TIMESTAMPTZ`, which compares as an instant already.
//! - SQLite has RFC 3339 *text*, which compares as text. `2026-10-01T10:00:00.4Z`
//!   is after `2026-10-01T10:00:00Z` as a time and before it as text (#216),
//!   so every comparison and ordering turns the column into a fixed-width UTC
//!   text first. The expression is the one in `migrator::m002_run_instant`'s
//!   index (SQLite uses an expression index only when the query's expression
//!   is the same): change one and you change the other, in a new step.

use sea_orm::DbBackend;
use sea_orm::sea_query::{Expr, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::types::StoreError;

/// Which database a query is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    Sqlite,
    Postgres,
}

impl From<DbBackend> for Dialect {
    fn from(backend: DbBackend) -> Self {
        match backend {
            DbBackend::Postgres => Self::Postgres,
            _ => Self::Sqlite,
        }
    }
}

impl Dialect {
    /// A time column as an expression that compares as an instant.
    ///
    /// RAW: SQLite's `substr` expression, which the query builder cannot
    /// spell.
    pub(crate) fn instant(self, column: &str) -> Expr {
        match self {
            Self::Sqlite => Expr::cust(format!(
                "substr({column}, 1, 19) || substr(CASE WHEN substr({column}, 20, 1) = '.' THEN substr({column}, 21, length({column}) - 21) ELSE '' END || '000000000', 1, 9)"
            )),
            Self::Postgres => Expr::cust(column.to_owned()),
        }
    }

    /// A bound to compare with [`Self::instant`], from RFC 3339 text.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] naming `column` when `value` is not a time.
    pub(crate) fn instant_value(
        self,
        column: &'static str,
        value: &str,
    ) -> Result<Value, StoreError> {
        let at = OffsetDateTime::parse(value, &Rfc3339)
            .map_err(|_| StoreError::Corrupt {
                column,
                value: value.to_owned(),
            })?
            .to_offset(time::UtcOffset::UTC);
        Ok(match self {
            Self::Sqlite => format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{:09}",
                at.year(),
                u8::from(at.month()),
                at.day(),
                at.hour(),
                at.minute(),
                at.second(),
                at.nanosecond()
            )
            .into(),
            Self::Postgres => at.into(),
        })
    }

    /// A time a caller gave as RFC 3339 text, to store; empty means now.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] naming `column` when `text` is not a time.
    pub(crate) fn time_text(self, column: &'static str, text: &str) -> Result<Value, StoreError> {
        if text.is_empty() {
            return Ok(self.now());
        }
        let at = OffsetDateTime::parse(text, &Rfc3339).map_err(|_| StoreError::Corrupt {
            column,
            value: text.to_owned(),
        })?;
        Ok(self.time(at))
    }

    /// A time to store: RFC 3339 text on SQLite, a timestamp on `PostgreSQL`.
    pub(crate) fn time(self, at: OffsetDateTime) -> Value {
        match self {
            Self::Sqlite => rfc3339(at).into(),
            Self::Postgres => at.into(),
        }
    }

    /// A time to store that may be absent, as a typed `NULL` when it is.
    pub(crate) fn time_opt(self, at: Option<OffsetDateTime>) -> Value {
        match self {
            Self::Sqlite => at.map(rfc3339).into(),
            Self::Postgres => at.into(),
        }
    }

    /// The current time, to store.
    pub(crate) fn now(self) -> Value {
        self.time(OffsetDateTime::now_utc())
    }
}

/// RFC 3339 text, the way the stores have always written and returned it.
pub(crate) fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
