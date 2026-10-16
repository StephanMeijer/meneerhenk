//! What a review found: the findings it posted, and the drafts the checker
//! looked at.

use henk_domain::run::RunId;
use sea_orm::sea_query::{Expr, ExprTrait, OnConflict, Order, Query};

use super::{Store, at, at_opt, get};
use crate::types::{
    DraftDecision, DraftRecord, FindingAction, FindingRecord, StoreError, draft_verdict, to_u32,
};

impl Store {
    pub(crate) async fn record_finding(
        &self,
        run: &RunId,
        session: &str,
        path: &str,
        line_number: u32,
        comment_id: &str,
        action: FindingAction,
    ) -> Result<(), StoreError> {
        let created = self.dialect().await?.now();
        let mut insert = Query::insert();
        insert
            .into_table("findings")
            .columns([
                "run_id",
                "session",
                "path",
                "line",
                "comment_id",
                "action",
                "created_at",
            ])
            .values([
                run.as_str().into(),
                session.into(),
                path.into(),
                i64::from(line_number).into(),
                comment_id.into(),
                action.as_str().into(),
                created.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "findings",
                value: error.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "created_at",
                "session",
                "path",
                "line",
                "comment_id",
                "action",
            ])
            .from("findings")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(FindingRecord {
                    at: at(row, 0)?,
                    session: get(row, 1)?,
                    path: get(row, 2)?,
                    line: to_u32("findings.line", get(row, 3)?)?,
                    comment_id: get(row, 4)?,
                    action: get(row, 5)?,
                })
            })
            .collect()
    }

    /// Records a draft; a draft recorded again keeps when it was made and what
    /// the checker said, and takes the new kind, target and body.
    pub(crate) async fn record_draft(
        &self,
        run: &RunId,
        draft: &DraftRecord,
    ) -> Result<(), StoreError> {
        let created = self
            .dialect()
            .await?
            .time_text("drafts.created_at", &draft.at)?;
        let mut insert = Query::insert();
        insert
            .into_table("drafts")
            .columns([
                "run_id",
                "draft",
                "session",
                "model",
                "kind",
                "path",
                "line",
                "target",
                "body",
                "created_at",
            ])
            .values([
                run.as_str().into(),
                draft.draft.as_str().into(),
                draft.session.as_str().into(),
                draft.model.as_str().into(),
                draft.kind.as_str().into(),
                draft.path.as_str().into(),
                i64::from(draft.line).into(),
                draft.target.as_str().into(),
                draft.body.as_str().into(),
                created.into(),
            ])
            .map_err(|error| StoreError::Corrupt {
                column: "drafts",
                value: error.to_string(),
            })?
            .on_conflict(
                OnConflict::columns(["run_id", "draft"])
                    .update_columns(["kind", "target", "body"])
                    .to_owned(),
            );
        self.exec(&insert).await.map(|_| ())
    }

    pub(crate) async fn decide_draft(
        &self,
        run: &RunId,
        draft: &str,
        decision: &DraftDecision,
    ) -> Result<(), StoreError> {
        let decided = self
            .dialect()
            .await?
            .time_text("drafts.decided_at", &decision.at)?;
        let mut update = Query::update();
        update
            .table("drafts")
            .values([
                ("verdict", decision.verdict.as_str().into()),
                ("checker", decision.checker.as_str().into()),
                ("reason", decision.reason.as_str().into()),
                ("same_as", decision.same_as.as_str().into()),
                ("comment_id", decision.comment_id.as_str().into()),
                ("decided_at", decided.into()),
            ])
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .and_where(Expr::col("draft").eq(draft));
        self.exec(&update).await.map(|_| ())
    }

    pub(crate) async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "created_at",
                "draft",
                "session",
                "model",
                "kind",
                "path",
                "line",
                "target",
                "body",
                "verdict",
                "checker",
                "reason",
                "same_as",
                "comment_id",
                "decided_at",
            ])
            .from("drafts")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                let verdict: Option<String> = get(row, 9)?;
                let decision = match verdict {
                    Some(verdict) => Some(DraftDecision {
                        at: at_opt(row, 14)?.unwrap_or_default(),
                        verdict: draft_verdict(&verdict)?,
                        checker: get(row, 10)?,
                        reason: get(row, 11)?,
                        same_as: get(row, 12)?,
                        comment_id: get(row, 13)?,
                    }),
                    None => None,
                };
                Ok(DraftRecord {
                    at: at(row, 0)?,
                    draft: get(row, 1)?,
                    session: get(row, 2)?,
                    model: get(row, 3)?,
                    kind: get(row, 4)?,
                    path: get(row, 5)?,
                    line: to_u32("drafts.line", get(row, 6)?)?,
                    target: get(row, 7)?,
                    body: get(row, 8)?,
                    decision,
                })
            })
            .collect()
    }
}
