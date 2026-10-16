//! The record of every tool call, and what it adds up to.

use henk_domain::run::RunId;
use sea_orm::sea_query::{
    Asterisk, Condition, Expr, ExprTrait, Func, JoinType, Order, Query, SelectStatement,
};

use super::dialect::Dialect;
use super::{Store, at, count, get};
use crate::types::{
    OutcomeFilter, Page, StoreError, ToolCallFilter, ToolCallListing, ToolCallRecord, ToolUsage,
    run_kind, run_platform, to_u32, to_u64,
};

/// A count that cannot be held is the most that can: no count gets there.
fn saturate(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Store {
    pub(crate) async fn record_tool_call(
        &self,
        run: &RunId,
        call: &ToolCallRecord,
    ) -> Result<(), StoreError> {
        let at = self.dialect().await?.time_text("tool_calls.at", &call.at)?;
        let mut insert = Query::insert();
        insert
            .into_table("tool_calls")
            .columns([
                "run_id",
                "session",
                "model",
                "turn",
                "tool",
                "origin",
                "outcome",
                "arguments",
                "arguments_len",
                "result_chars",
                "elapsed_ms",
                "at",
            ])
            .values([
                run.as_str().into(),
                call.session.as_str().into(),
                call.model.as_str().into(),
                i64::from(call.turn).into(),
                call.tool.as_str().into(),
                call.origin.as_str().into(),
                call.outcome.as_str().into(),
                call.arguments.as_str().into(),
                saturate(call.arguments_len).into(),
                saturate(call.result_chars).into(),
                saturate(call.elapsed_ms).into(),
                at.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "tool_calls",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "at",
                "session",
                "model",
                "turn",
                "tool",
                "origin",
                "outcome",
                "arguments",
                "arguments_len",
                "result_chars",
                "elapsed_ms",
            ])
            .from("tool_calls")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| call_record(row, 0))
            .collect()
    }

    /// Calls added up per model, session role and tool, for the calls
    /// `filter` matches. Its outcome and keyset do not apply.
    pub(crate) async fn tool_usage(
        &self,
        filter: &ToolCallFilter,
    ) -> Result<Vec<ToolUsage>, StoreError> {
        let mut query = Query::select();
        calls(&mut query);
        query
            .column(("c", "model"))
            .expr(call_role())
            .expr(call_tool())
            .column(("c", "outcome"))
            .expr(Func::count(Expr::col(Asterisk)))
            .expr(
                Func::coalesce([Expr::col(("c", "elapsed_ms")).sum(), Expr::val(0)])
                    .cast_as("bigint"),
            )
            .cond_where(call_conditions(self.dialect().await?, filter)?)
            // RAW: the role and the tool are expressions, grouped by position.
            .add_group_by([
                Expr::col(("c", "model")),
                Expr::cust("2"),
                Expr::cust("3"),
                Expr::col(("c", "outcome")),
            ]);
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok((
                    get(row, 0)?,
                    get(row, 1)?,
                    get(row, 2)?,
                    get(row, 3)?,
                    count(row, 4)?,
                    count(row, 5)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(ToolUsage::across_runs(rows))
    }

    /// Calls newest first, for the dashboard: filtered, paged by offset or by
    /// the key of the last one seen. The order is the stored text of `at`,
    /// as it has always been.
    pub(crate) async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError> {
        let dialect = self.dialect().await?;
        let mut conditions = call_conditions(dialect, filter)?;
        match filter.outcome.as_ref() {
            Some(OutcomeFilter::Is(outcome)) => {
                conditions = conditions.add(Expr::col(("c", "outcome")).eq(outcome.as_str()));
            }
            Some(OutcomeFilter::Problems) => {
                conditions = conditions.add(Expr::col(("c", "outcome")).ne("ok"));
            }
            None => {}
        }
        if let Some(key) = &filter.before {
            let at = dialect.time_text("before", &key.at)?;
            conditions = conditions.add(
                Condition::any()
                    .add(Expr::col(("c", "at")).lt(at.clone()))
                    .add(
                        Condition::all()
                            .add(Expr::col(("c", "at")).eq(at))
                            .add(Expr::col(("c", "id")).lt(key.id)),
                    ),
            );
        }
        let kept = Query::select()
            .expr(Expr::val(1))
            .from_as("transcripts", "t")
            .and_where(Expr::col(("t", "run_id")).equals(("c", "run_id")))
            .and_where(Expr::col(("t", "session")).equals(("c", "session")))
            .take();
        let mut query = Query::select();
        calls(&mut query);
        query
            .join_as(
                JoinType::InnerJoin,
                "runs",
                "r",
                Expr::col(("r", "id")).equals(("c", "run_id")),
            )
            .columns([
                ("c", "id"),
                ("c", "run_id"),
                ("r", "repo"),
                ("r", "target"),
                ("r", "platform"),
                ("c", "at"),
                ("c", "session"),
                ("c", "model"),
                ("c", "turn"),
                ("c", "tool"),
                ("c", "origin"),
                ("c", "outcome"),
                ("c", "arguments"),
                ("c", "arguments_len"),
                ("c", "result_chars"),
                ("c", "elapsed_ms"),
            ])
            .expr(Expr::exists(kept))
            .column(("r", "kind"))
            .cond_where(conditions)
            .order_by(("c", "at"), Order::Desc)
            .order_by(("c", "id"), Order::Desc)
            .limit(page.limit().into())
            .offset(page.offset().into());
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(ToolCallListing {
                    id: get(row, 0)?,
                    run_id: get(row, 1)?,
                    repo: get(row, 2)?,
                    target: to_u64("runs.target", get(row, 3)?)?,
                    platform: run_platform(get(row, 4)?)?,
                    kind: run_kind(get(row, 17)?)?,
                    call: call_record(row, 5)?,
                    transcript_kept: get(row, 16)?,
                })
            })
            .collect()
    }
}

/// A tool call from eleven consecutive columns starting at `first`: `at,
/// session, model, turn, tool, origin, outcome, arguments, arguments_len,
/// result_chars, elapsed_ms`. A run's calls select exactly these, and a
/// listing selects them after its five columns about the run.
fn call_record(row: &sea_orm::QueryResult, first: usize) -> Result<ToolCallRecord, StoreError> {
    Ok(ToolCallRecord {
        at: at(row, first)?,
        session: get(row, first + 1)?,
        model: get(row, first + 2)?,
        turn: to_u32("tool_calls.turn", get(row, first + 3)?)?,
        tool: get(row, first + 4)?,
        origin: get(row, first + 5)?,
        outcome: get(row, first + 6)?,
        arguments: get(row, first + 7)?,
        arguments_len: count(row, first + 8)?,
        result_chars: count(row, first + 9)?,
        elapsed_ms: count(row, first + 10)?,
    })
}

/// The `WHERE` of a tool call tally or listing, over `tool_calls c` and
/// `sessions s`.
fn call_conditions(dialect: Dialect, filter: &ToolCallFilter) -> Result<Condition, StoreError> {
    let mut all = Condition::all();
    if let Some(tool) = &filter.tool {
        all = all.add(call_tool().eq(tool.as_str()));
    }
    if let Some(model) = &filter.model {
        all = all.add(Expr::col(("c", "model")).eq(model.as_str()));
    }
    if let Some(role) = filter.role {
        all = all.add(call_role().eq(role.as_str()));
    }
    if let Some(since) = &filter.since {
        all = all.add(
            dialect
                .instant("c.at")
                .gte(dialect.instant_value("since", since)?),
        );
    }
    if let Some(until) = &filter.until {
        all = all.add(
            dialect
                .instant("c.at")
                .lt(dialect.instant_value("until", until)?),
        );
    }
    Ok(all)
}

/// `tool_calls c LEFT JOIN sessions s`: the role of the session that made a
/// call.
fn calls(query: &mut SelectStatement) {
    query.from_as("tool_calls", "c").join_as(
        JoinType::LeftJoin,
        "sessions",
        "s",
        Condition::all()
            .add(Expr::col(("s", "run_id")).equals(("c", "run_id")))
            .add(Expr::col(("s", "name")).equals(("c", "session"))),
    );
}

/// The tool a call `c` counts under, as [`crate::types::counted_as`] says:
/// every tool that does not exist, which is every call with no origin, as
/// one (#341).
fn call_tool() -> Expr {
    Expr::case(Expr::col(("c", "origin")).eq(""), "(unknown)")
        .finally(Expr::col(("c", "tool")))
        .into()
}

/// The role of the session that made a call `c`; a call whose session has
/// no row counts as a lane's.
fn call_role() -> Expr {
    Func::coalesce([Expr::col(("s", "role")), Expr::val("lane")]).into()
}
