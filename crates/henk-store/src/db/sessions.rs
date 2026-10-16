//! The sessions of a run, their rounds, and the sessions of the newest
//! reviews.

use henk_domain::run::{RunId, RunKind};
use sea_orm::sea_query::{Expr, ExprTrait, JoinType, Order, Query, SelectStatement};
use time::OffsetDateTime;

use super::dialect::{Dialect, rfc3339};
use super::{Store, at, at_opt, get};
use crate::types::{
    MOST_SESSION_REVIEWS, RoundRecord, SessionEnding, SessionRecord, SessionRole, SessionStatus,
    StoreError, TokenUsage, kind_str, round_record, session_dots, session_endings, session_role,
    session_status, to_i64, to_u64,
};

/// A count that cannot be held is the most that can: no count gets there.
fn saturate(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Store {
    pub(crate) async fn start_session(
        &self,
        run: &RunId,
        name: &str,
        role: SessionRole,
        model: &str,
    ) -> Result<(), StoreError> {
        let started = self.dialect().await?.now();
        let mut insert = Query::insert();
        insert
            .into_table("sessions")
            .columns(["run_id", "name", "role", "model", "status", "started_at"])
            .values([
                run.as_str().into(),
                name.into(),
                role.as_str().into(),
                model.into(),
                SessionStatus::Running.as_str().into(),
                started.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "sessions",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn finish_session(
        &self,
        run: &RunId,
        name: &str,
        status: SessionStatus,
        turns: u64,
        tokens: TokenUsage,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let finished = self.dialect().await?.now();
        let mut update = Query::update();
        update
            .table("sessions")
            .values([
                ("status", status.as_str().into()),
                ("turns", saturate(turns).into()),
                ("input_tokens", saturate(tokens.input).into()),
                ("output_tokens", saturate(tokens.output).into()),
                ("finished_at", finished.into()),
                ("error", error.map(str::to_owned).into()),
                ("cache_read_tokens", saturate(tokens.cache_read).into()),
                ("cache_write_tokens", saturate(tokens.cache_write).into()),
                ("reasoning_tokens", saturate(tokens.reasoning).into()),
            ])
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .and_where(Expr::col("name").eq(name));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn sessions(&self, run: &RunId) -> Result<Vec<SessionRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "name",
                "model",
                "status",
                "turns",
                "input_tokens",
                "output_tokens",
                "error",
                "started_at",
                "finished_at",
                "cache_read_tokens",
                "cache_write_tokens",
                "role",
                "reasoning_tokens",
            ])
            .from("sessions")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("name", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(SessionRecord {
                    name: get(row, 0)?,
                    role: session_role(get(row, 11)?)?,
                    model: get(row, 1)?,
                    status: session_status(get(row, 2)?)?,
                    turns: to_u64("sessions.turns", get(row, 3)?)?,
                    input_tokens: to_u64("sessions.input_tokens", get(row, 4)?)?,
                    output_tokens: to_u64("sessions.output_tokens", get(row, 5)?)?,
                    cache_read_tokens: to_u64("sessions.cache_read_tokens", get(row, 9)?)?,
                    cache_write_tokens: to_u64("sessions.cache_write_tokens", get(row, 10)?)?,
                    reasoning_tokens: to_u64("sessions.reasoning_tokens", get(row, 12)?)?,
                    error: get(row, 6)?,
                    started_at: at(row, 7)?,
                    finished_at: at_opt(row, 8)?,
                })
            })
            .collect()
    }

    /// The name, role and status of every session of `runs`, for a listing.
    pub(crate) async fn sessions_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, SessionRole, SessionStatus)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = Query::select();
        query
            .columns(["run_id", "name", "role", "status"])
            .from("sessions")
            .and_where(Expr::col("run_id").is_in(runs.iter().map(RunId::as_str)))
            .order_by("run_id", Order::Asc)
            .order_by("name", Order::Asc);
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?, get(row, 2)?, get(row, 3)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        session_dots(rows)
    }

    /// The sessions of the newest `reviews` reviews that ended, newest
    /// first, for the ones started at or after `since`.
    pub(crate) async fn session_endings(
        &self,
        since: Option<OffsetDateTime>,
        reviews: u32,
    ) -> Result<Vec<SessionEnding>, StoreError> {
        let query = session_endings_query(self.dialect().await?, since, reviews)?;
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok((
                    get(row, 0)?,
                    at(row, 1)?,
                    get(row, 2)?,
                    get(row, 3)?,
                    get(row, 4)?,
                    get(row, 5)?,
                    get(row, 6)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        session_endings(rows)
    }

    pub(crate) async fn start_round(
        &self,
        run: &RunId,
        round: &RoundRecord,
    ) -> Result<(), StoreError> {
        let dialect = self.dialect().await?;
        let started = dialect.time_text("session_rounds.started_at", &round.started_at)?;
        let handover_len = to_i64("session_rounds.handover_len", round.handover_len)?;
        let mut insert = Query::insert();
        insert
            .into_table("session_rounds")
            .columns([
                "run_id",
                "session",
                "role",
                "round",
                "started_at",
                "first_turn",
                "handover",
                "handover_len",
            ])
            .values([
                run.as_str().into(),
                round.session.as_str().into(),
                round.role.as_str().into(),
                i64::from(round.round).into(),
                started.into(),
                i64::from(round.first_turn).into(),
                round.handover.as_str().into(),
                handover_len.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "session_rounds",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn end_round(
        &self,
        run: &RunId,
        session: &str,
        round: u32,
        turns: u32,
    ) -> Result<(), StoreError> {
        let ended = self.dialect().await?.now();
        let mut update = Query::update();
        update
            .table("session_rounds")
            .values([
                ("ended_at", ended.into()),
                ("turns", i64::from(turns).into()),
            ])
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .and_where(Expr::col("session").eq(session))
            .and_where(Expr::col("round").eq(i64::from(round)));
        self.exec(&update).await.map(|_| ())
    }

    /// The rounds of a run, in the order they started, as instants.
    pub(crate) async fn rounds(&self, run: &RunId) -> Result<Vec<RoundRecord>, StoreError> {
        let query = rounds_query(self.dialect().await?, run);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                round_record((
                    get(row, 0)?,
                    get(row, 1)?,
                    get(row, 2)?,
                    at(row, 3)?,
                    at_opt(row, 4)?,
                    get(row, 5)?,
                    get(row, 6)?,
                    get(row, 7)?,
                    get(row, 8)?,
                ))
            })
            .collect()
    }
}

/// The rounds of `run`: by when they started as an instant, not as text
/// (#216), then session and round.
pub(super) fn rounds_query(dialect: Dialect, run: &RunId) -> SelectStatement {
    let mut query = Query::select();
    query
        .columns([
            "session",
            "role",
            "round",
            "started_at",
            "ended_at",
            "first_turn",
            "turns",
            "handover",
            "handover_len",
        ])
        .from("session_rounds")
        .and_where(Expr::col("run_id").eq(run.as_str()))
        .order_by_expr(dialect.instant("started_at"), Order::Asc)
        .order_by("session", Order::Asc)
        .order_by("round", Order::Asc);
    query
}

/// The sessions of the newest `reviews` ended reviews. The reviews are picked
/// and ordered as instants, so runs started in the same second keep their
/// order (#216), then each is joined to its sessions.
pub(super) fn session_endings_query(
    dialect: Dialect,
    since: Option<OffsetDateTime>,
    reviews: u32,
) -> Result<SelectStatement, StoreError> {
    let mut newest = Query::select();
    newest
        .columns(["id", "started_at"])
        .expr_as(dialect.instant("started_at"), "at")
        .from("runs")
        .and_where(Expr::col("kind").eq(kind_str(RunKind::Review)))
        .and_where(Expr::col("status").is_not_in(["running", "superseded", "cancelled"]))
        .order_by_expr(dialect.instant("started_at"), Order::Desc)
        .order_by("id", Order::Desc)
        .limit(u64::from(Ord::min(reviews, MOST_SESSION_REVIEWS)));
    if let Some(since) = since {
        newest.and_where(
            dialect
                .instant("started_at")
                .gte(dialect.instant_value("since", &rfc3339(since))?),
        );
    }
    let mut query = Query::select();
    query
        .column(("r", "id"))
        .column(("r", "started_at"))
        .columns([
            ("s", "name"),
            ("s", "role"),
            ("s", "model"),
            ("s", "status"),
            ("s", "error"),
        ])
        .from_subquery(newest, "r")
        .join_as(
            JoinType::InnerJoin,
            "sessions",
            "s",
            Expr::col(("s", "run_id")).equals(("r", "id")),
        )
        .order_by(("r", "at"), Order::Desc)
        .order_by(("r", "id"), Order::Desc)
        .order_by(("s", "name"), Order::Asc);
    Ok(query)
}
