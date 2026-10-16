//! The counts the overview shows.

use sea_orm::sea_query::{Asterisk, Expr, ExprTrait, Func, Query};
use time::OffsetDateTime;

use super::dialect::rfc3339;
use super::{Store, get};
use crate::types::{DayCounts, StoreError, merge_days};

impl Store {
    /// Runs, findings and drafts per UTC day since `since`. Findings count
    /// as posted when they were posted or could not be placed; the drafts of
    /// a review loop, a plan review and a requirements review are left out
    /// of the drafts (#285).
    pub(crate) async fn daily_stats(
        &self,
        since: OffsetDateTime,
    ) -> Result<Vec<DayCounts>, StoreError> {
        let dialect = self.dialect().await?;
        let since = dialect.instant_value("since", &rfc3339(since))?;
        let status_is = |status: &'static str| {
            Expr::case(Expr::col("status").eq(status), 1)
                .finally(0)
                .sum()
        };

        let mut runs = Query::select();
        runs.expr(dialect.day("started_at"))
            .expr(Func::count(Expr::col(Asterisk)))
            .expr(status_is("finished"))
            .expr(status_is("failed"))
            .from("runs")
            .and_where(dialect.instant("started_at").gte(since.clone()))
            // RAW: grouped by the first column, the day expression.
            .add_group_by([Expr::cust("1")]);
        let runs = self
            .all(&runs)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?, get(row, 2)?, get(row, 3)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;

        let mut findings = Query::select();
        findings
            .expr(dialect.day("created_at"))
            .expr(Func::count(Expr::col(Asterisk)))
            .from("findings")
            .and_where(Expr::col("action").is_in(["posted", "unverified"]))
            .and_where(dialect.instant("created_at").gte(since.clone()))
            .add_group_by([Expr::cust("1")]);
        let findings = self
            .all(&findings)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;

        let mut drafts = Query::select();
        drafts
            .expr(dialect.day("created_at"))
            .expr(Func::count(Expr::col(Asterisk)))
            .from("drafts")
            .and_where(dialect.instant("created_at").gte(since))
            .and_where(Expr::col("kind").is_not_in([
                "loop_finding",
                "plan_point",
                "requirement_point",
            ]))
            .add_group_by([Expr::cust("1")]);
        let drafts = self
            .all(&drafts)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(merge_days(runs, findings, drafts))
    }
}
