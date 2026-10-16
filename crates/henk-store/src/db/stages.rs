//! The stages a run passes through, and the runs it started.

use henk_domain::run::RunId;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict, Query};

use super::{Store, at, at_opt, get};
use crate::types::{
    Stage, StageRecord, StageState, StageWrite, StoreError, child_records, stage_dots,
    stage_records,
};

impl Store {
    /// Records where a stage stands. The first write of a stage says when it
    /// started; later writes change its state, end and detail only.
    pub(crate) async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError> {
        let dialect = self.dialect().await?;
        let now = dialect.now();
        let started = write
            .started_at
            .map_or_else(|| now.clone(), |t| dialect.time(t));
        let ended = if write.state.ended() {
            write.ended_at.map_or_else(|| now, |t| dialect.time(t))
        } else {
            dialect.time_opt(None)
        };
        let mut insert = Query::insert();
        insert
            .into_table("stages")
            .columns([
                "run_id",
                "stage",
                "state",
                "started_at",
                "ended_at",
                "detail",
            ])
            .values([
                run.as_str().into(),
                write.stage.as_str().into(),
                write.state.as_str().into(),
                started.into(),
                ended.into(),
                write.detail.as_str().into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "stages",
                value: error.to_string(),
            })?
            .on_conflict(
                OnConflict::columns(["run_id", "stage"])
                    .update_columns(["state", "ended_at", "detail"])
                    .to_owned(),
            );
        self.exec(&insert).await.map(|_| ())
    }

    /// Ends the stages of `run` that were still running, as failed.
    pub(crate) async fn fail_running_stages(
        &self,
        run: &RunId,
        reason: &str,
    ) -> Result<(), StoreError> {
        let ended = self.dialect().await?.now();
        let mut update = Query::update();
        update
            .table("stages")
            .values([
                ("state", StageState::Failed.as_str().into()),
                ("ended_at", ended.into()),
                ("detail", reason.into()),
            ])
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .and_where(Expr::col("state").eq(StageState::Running.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["stage", "state", "started_at", "ended_at", "detail"])
            .from("stages")
            .and_where(Expr::col("run_id").eq(run.as_str()));
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok((
                    get(row, 0)?,
                    get(row, 1)?,
                    at(row, 2)?,
                    at_opt(row, 3)?,
                    get(row, 4)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_records(rows)
    }

    /// The stage and state of every stage of `runs`, for a listing.
    pub(crate) async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = Query::select();
        query
            .columns(["run_id", "stage", "state"])
            .from("stages")
            .and_where(Expr::col("run_id").is_in(runs.iter().map(RunId::as_str)));
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?, get(row, 2)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_dots(rows)
    }

    /// Records that the run started for `stage` of `parent` is `child`; a
    /// second run for the same stage replaces the first.
    pub(crate) async fn link_child(
        &self,
        parent: &RunId,
        stage: Stage,
        child: &RunId,
    ) -> Result<(), StoreError> {
        let mut insert = Query::insert();
        insert
            .into_table("run_children")
            .columns(["parent", "stage", "child"])
            .values([
                parent.as_str().into(),
                stage.as_str().into(),
                child.as_str().into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "run_children",
                value: error.to_string(),
            })?
            .on_conflict(
                OnConflict::columns(["parent", "stage"])
                    .update_column("child")
                    .to_owned(),
            );
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn children(&self, parent: &RunId) -> Result<Vec<(Stage, RunId)>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["stage", "child"])
            .from("run_children")
            .and_where(Expr::col("parent").eq(parent.as_str()));
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        child_records(rows)
    }
}
