//! The runs: writing and finishing them, reading one, and the listings.

use henk_domain::run::{RunId, RunKind};
use sea_orm::QueryResult;
use sea_orm::sea_query::{
    Asterisk, Condition, Expr, ExprTrait, Func, Order, Query, SelectStatement,
};
use time::OffsetDateTime;

use super::dialect::{Dialect, rfc3339};
use super::{Store, at, at_opt, count, get};
use crate::types::{
    NewRun, Page, RawRun, RunFilter, RunRecord, RunStatus, SessionStatus, StoreError, kind_str,
    platform_str, to_i64,
};

impl Store {
    pub(crate) async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        let target = to_i64("runs.target", run.target)?;
        let started = self.dialect().await?.now();
        let mut insert = Query::insert();
        insert
            .into_table("runs")
            .columns([
                "id",
                "kind",
                "platform",
                "repo",
                "target",
                "commit_sha",
                "requester",
                "trigger",
                "status",
                "started_at",
                "link",
                "heartbeat_at",
            ])
            .values([
                run.id.as_str().into(),
                kind_str(run.kind).into(),
                platform_str(run.platform).into(),
                run.repo.as_str().into(),
                target.into(),
                run.commit.clone().into(),
                run.requester.clone().into(),
                run.trigger.as_str().into(),
                RunStatus::Running.as_str().into(),
                started.clone().into(),
                run.link.as_str().into(),
                started.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "runs",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let finished = self.dialect().await?.now();
        let mut update = Query::update();
        update
            .table("runs")
            .values([
                ("status", status.as_str().into()),
                ("finished_at", finished.into()),
                ("summary", summary.map(str::to_owned).into()),
                ("error", error.map(str::to_owned).into()),
            ])
            .and_where(Expr::col("id").eq(id.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn supersede_run(
        &self,
        id: &RunId,
        by: &RunId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let finished = self.dialect().await?.now();
        let mut update = Query::update();
        update
            .table("runs")
            .values([
                ("status", RunStatus::Superseded.as_str().into()),
                ("finished_at", finished.into()),
                ("error", reason.into()),
                ("superseded_by", by.as_str().into()),
            ])
            .and_where(Expr::col("id").eq(id.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        let mut query = run_select();
        query
            .from("runs")
            .and_where(Expr::col("id").eq(id.as_str()));
        self.first(&query)
            .await?
            .map(|row| raw_run(&row)?.into_record())
            .transpose()
    }

    pub(crate) async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
        let at = self.dialect().await?.now();
        let mut update = Query::update();
        update
            .table("runs")
            .values([("heartbeat_at", at.into())])
            .and_where(Expr::col("id").eq(id.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn end_loop(
        &self,
        id: &RunId,
        stop: &str,
        rounds: u32,
    ) -> Result<(), StoreError> {
        let mut update = Query::update();
        update
            .table("runs")
            .values([
                ("loop_stop", stop.into()),
                ("loop_rounds", i64::from(rounds).into()),
            ])
            .and_where(Expr::col("id").eq(id.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
        let mut update = Query::update();
        update
            .table("runs")
            .values([("check_id", check_id.into())])
            .and_where(Expr::col("id").eq(id.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    /// Running runs with no heartbeat, or one before `stale_before`: the ones
    /// a process that died left behind.
    pub(crate) async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let dialect = self.dialect().await?;
        let cutoff = dialect.instant_value("stale_before", &rfc3339(stale_before))?;
        let mut query = run_select();
        query
            .from("runs")
            .and_where(Expr::col("status").eq(RunStatus::Running.as_str()))
            .cond_where(
                Condition::any()
                    .add(Expr::col("heartbeat_at").is_null())
                    .add(dialect.instant("heartbeat_at").lt(cutoff)),
            )
            .order_by_expr(dialect.instant("started_at"), Order::Asc)
            .order_by("id", Order::Asc);
        self.runs(&query).await
    }

    /// The newest review of each pull request that started at or after
    /// `since`, compared as instants.
    pub(crate) async fn latest_reviews(
        &self,
        since: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let dialect = self.dialect().await?;
        let since = dialect.instant_value("since", &rfc3339(since))?;
        let newer = Query::select()
            .expr(Expr::val(1))
            .from_as("runs", "n")
            .and_where(Expr::col(("n", "kind")).equals(("r", "kind")))
            .and_where(Expr::col(("n", "platform")).equals(("r", "platform")))
            .and_where(Expr::col(("n", "repo")).equals(("r", "repo")))
            .and_where(Expr::col(("n", "target")).equals(("r", "target")))
            .cond_where(
                Condition::any()
                    .add(
                        dialect
                            .instant("n.started_at")
                            .gt(dialect.instant("r.started_at")),
                    )
                    .add(
                        Condition::all()
                            .add(
                                dialect
                                    .instant("n.started_at")
                                    .eq(dialect.instant("r.started_at")),
                            )
                            .add(Expr::col(("n", "id")).gt(Expr::col(("r", "id")))),
                    ),
            )
            .take();
        let mut query = run_select();
        query
            .from_as("runs", "r")
            .and_where(Expr::col(("r", "kind")).eq(kind_str(RunKind::Review)))
            .and_where(dialect.instant("r.started_at").gte(since))
            .and_where(Expr::not_exists(newer))
            .order_by_expr(dialect.instant("r.started_at"), Order::Asc)
            .order_by(("r", "id"), Order::Asc);
        self.runs(&query).await
    }

    /// Takes over a run that failed as interrupted: true for exactly one
    /// caller, however many try at once.
    pub(crate) async fn claim_resume(
        &self,
        run: &RunId,
        interrupted: &str,
        resumed: &str,
    ) -> Result<bool, StoreError> {
        let mut update = Query::update();
        update
            .table("runs")
            .values([("error", resumed.into())])
            .and_where(Expr::col("id").eq(run.as_str()))
            .and_where(Expr::col("status").eq(RunStatus::Failed.as_str()))
            .and_where(Expr::col("error").eq(interrupted));
        Ok(self.exec(&update).await? == 1)
    }

    /// Ends the sessions of `run` that were still running when it did not.
    pub(crate) async fn drop_running_sessions(
        &self,
        run: &RunId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let finished = self.dialect().await?.now();
        let mut update = Query::update();
        update
            .table("sessions")
            .values([
                ("status", SessionStatus::DidNotFinish.as_str().into()),
                ("finished_at", finished.into()),
                ("error", reason.into()),
            ])
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .and_where(Expr::col("status").eq(SessionStatus::Running.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let query = list_runs_query(self.dialect().await?, filter, page)?;
        self.runs(&query).await
    }

    pub(crate) async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        let mut query = Query::select();
        query
            .expr(Func::count(Expr::col(Asterisk)))
            .from("runs")
            .cond_where(run_conditions(self.dialect().await?, filter)?);
        self.first(&query)
            .await?
            .map_or(Ok(0), |row| count(&row, 0))
    }

    async fn runs(&self, query: &SelectStatement) -> Result<Vec<RunRecord>, StoreError> {
        self.all(query)
            .await?
            .iter()
            .map(|row| raw_run(row)?.into_record())
            .collect()
    }
}

/// The query of a run listing: newest first as an instant, then by id.
pub(super) fn list_runs_query(
    dialect: Dialect,
    filter: &RunFilter,
    page: Page,
) -> Result<SelectStatement, StoreError> {
    let mut query = run_select();
    query
        .from("runs")
        .cond_where(run_conditions(dialect, filter)?)
        .order_by_expr(dialect.instant("started_at"), Order::Desc)
        .order_by("id", Order::Desc)
        .limit(page.limit().into())
        .offset(page.offset().into());
    Ok(query)
}

/// The `WHERE` of a run listing. Only the parts the filter sets are added,
/// so there is no `(? IS NULL OR ...)`.
fn run_conditions(dialect: Dialect, filter: &RunFilter) -> Result<Condition, StoreError> {
    let mut all = Condition::all();
    if let Some(kind) = filter.kind {
        all = all.add(Expr::col("kind").eq(kind_str(kind)));
    }
    if let Some(status) = filter.status {
        all = all.add(Expr::col("status").eq(status.as_str()));
    }
    if let Some(platform) = filter.platform {
        all = all.add(Expr::col("platform").eq(platform_str(platform)));
    }
    if let Some(repo) = &filter.repo {
        all = all.add(Expr::col("repo").eq(repo.as_str()));
    }
    if let Some(target) = filter.target {
        // Above i64::MAX no run has it: the filter matches nothing.
        all = all.add(Expr::col("target").eq(i64::try_from(target).unwrap_or(i64::MAX)));
    }
    if let Some(since) = &filter.since {
        all = all.add(
            dialect
                .instant("started_at")
                .gte(dialect.instant_value("since", since)?),
        );
    }
    if let Some(until) = &filter.until {
        all = all.add(
            dialect
                .instant("started_at")
                .lt(dialect.instant_value("until", until)?),
        );
    }
    if let Some(key) = &filter.before {
        let at = dialect.instant_value("before", &key.started_at)?;
        all = all.add(
            Condition::any()
                .add(dialect.instant("started_at").lt(at.clone()))
                .add(
                    Condition::all()
                        .add(dialect.instant("started_at").eq(at))
                        .add(Expr::col("id").lt(key.id.as_str())),
                ),
        );
    }
    Ok(all)
}

/// The columns [`raw_run`] reads, in order. `loop_rounds` is `INTEGER` (an
/// `i32` to the driver) on `PostgreSQL` and 64 bits on SQLite: cast, so one
/// type reads both.
fn run_select() -> SelectStatement {
    let mut query = Query::select();
    query
        .columns([
            "id",
            "kind",
            "platform",
            "repo",
            "target",
            "commit_sha",
            "requester",
            "trigger",
            "status",
            "started_at",
            "finished_at",
            "link",
            "summary",
            "error",
            "heartbeat_at",
            "check_id",
            "superseded_by",
            "loop_stop",
        ])
        .expr(Expr::col("loop_rounds").cast_as("bigint"));
    query
}

fn raw_run(row: &QueryResult) -> Result<RawRun, StoreError> {
    Ok(RawRun {
        id: get(row, 0)?,
        kind: get(row, 1)?,
        platform: get(row, 2)?,
        repo: get(row, 3)?,
        target: get(row, 4)?,
        commit: get(row, 5)?,
        requester: get(row, 6)?,
        trigger: get(row, 7)?,
        status: get(row, 8)?,
        started_at: at(row, 9)?,
        finished_at: at_opt(row, 10)?,
        link: get(row, 11)?,
        summary: get(row, 12)?,
        error: get(row, 13)?,
        heartbeat_at: at_opt(row, 14)?,
        check_id: get(row, 15)?,
        superseded_by: get(row, 16)?,
        loop_stop: get(row, 17)?,
        loop_rounds: get(row, 18)?,
    })
}
