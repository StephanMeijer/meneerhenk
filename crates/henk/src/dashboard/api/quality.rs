//! Review quality (#205): what became of the lanes' drafts across runs,
//! per model, lane, repository or pull request, and the drafts behind the
//! numbers. The rejection rate is rejected of judged, where judged is what
//! a checker decided: confirmed, rejected and repeats.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_store::{DraftFilter, DraftGroup, DraftKey, DraftRates, DraftVerdict, VerdictFilter};
use serde::Deserialize;

use super::types::{Draft, DraftItem, Page, QualityRow, link_to};
use super::{ApiError, ApiQuery, ApiResult, cursor, limit, read_cursor, read_time};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;

/// What `GET /quality` and `GET /drafts` take. Every filter is optional.
#[derive(Debug, Default, Deserialize)]
pub struct QualityQuery {
    group: Option<String>,
    verdict: Option<String>,
    model: Option<String>,
    lane: Option<String>,
    repo: Option<String>,
    since: Option<String>,
    until: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

fn given(value: Option<&String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

impl QualityQuery {
    fn filter(&self) -> Result<DraftFilter, ApiError> {
        let verdict = match given(self.verdict.as_ref()).as_deref() {
            None => None,
            Some("waiting") => Some(VerdictFilter::Waiting),
            Some(name) => Some(VerdictFilter::Is(DraftVerdict::parse(name).ok_or_else(|| {
                ApiError::bad_request(
                    "verdict is one of confirmed, rejected, same_as, unchecked, not_checked, cancelled, failed, waiting.",
                )
            })?)),
        };
        let before = match self.cursor.as_deref() {
            None => None,
            Some(text) => {
                let (created_at, id) = read_cursor(text)?;
                let id = id.parse().map_err(|_| {
                    ApiError::bad_request("The cursor is not one this API gave out.")
                })?;
                Some(DraftKey { created_at, id })
            }
        };
        Ok(DraftFilter {
            verdict,
            model: given(self.model.as_ref()),
            lane: given(self.lane.as_ref()),
            repo: given(self.repo.as_ref()),
            since: self
                .since
                .as_deref()
                .map(|v| read_time("since", v))
                .transpose()?,
            until: self
                .until
                .as_deref()
                .map(|v| read_time("until", v))
                .transpose()?,
            before,
        })
    }

    fn group(&self) -> Result<DraftGroup, ApiError> {
        match self.group.as_deref().unwrap_or("model") {
            "model" => Ok(DraftGroup::Model),
            "lane" => Ok(DraftGroup::Lane),
            "repo" => Ok(DraftGroup::Repo),
            "target" => Ok(DraftGroup::Target),
            _ => Err(ApiError::bad_request(
                "group is one of model, lane, repo, target.",
            )),
        }
    }
}

/// `GET /quality`: what became of the drafts, per group, the largest
/// group first.
pub async fn rates(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<QualityQuery>,
) -> ApiResult<Vec<QualityRow>> {
    let group = query.group()?;
    let filter = query.filter()?;
    let rows = dashboard.app.store.draft_rates(group, &filter).await?;
    let settings = &dashboard.app.settings;
    Ok(Json(
        rows.into_iter().map(|rates| row(settings, rates)).collect(),
    ))
}

fn row(settings: &crate::config::Settings, rates: DraftRates) -> QualityRow {
    let judged = rates.confirmed + rates.rejected + rates.same_as;
    let rejection_rate = (judged > 0).then(|| {
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts of drafts are far below 2^52"
        )]
        let rate = rates.rejected as f64 / judged as f64;
        rate
    });
    let target_url = match (rates.platform, rates.repo.as_deref(), rates.target) {
        (Some(platform), Some(repo), Some(target)) => {
            link_to(settings, platform, repo, target, false)
        }
        _ => None,
    };
    QualityRow {
        key: rates.key,
        repo: rates.repo,
        target: rates.target,
        target_url,
        drafts: rates.drafts,
        confirmed: rates.confirmed,
        rejected: rates.rejected,
        same_as: rates.same_as,
        unchecked: rates.unchecked,
        not_checked: rates.not_checked,
        cancelled: rates.cancelled,
        failed: rates.failed,
        waiting: rates.waiting,
        judged,
        rejection_rate,
    }
}

/// `GET /drafts`: drafts across runs, newest first, a page at a time.
pub async fn drafts(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<QualityQuery>,
) -> ApiResult<Page<DraftItem>> {
    let filter = query.filter()?;
    let limit = limit(query.limit);
    let listed = dashboard
        .app
        .store
        .list_drafts(&filter, henk_store::Page::new(limit, 0))
        .await?;
    let next = (u32::try_from(listed.len()).ok() == Some(limit))
        .then(|| {
            listed
                .last()
                .map(|d| cursor(&d.draft.at, &d.id.to_string()))
        })
        .flatten();
    let settings = &dashboard.app.settings;
    Ok(Json(Page {
        items: listed
            .iter()
            .map(|listing| DraftItem {
                run_id: listing.run_id.clone(),
                repo: listing.repo.clone(),
                target: listing.target,
                target_url: link_to(
                    settings,
                    listing.platform,
                    &listing.repo,
                    listing.target,
                    false,
                ),
                draft: Draft::from(&listing.draft),
            })
            .collect(),
        next,
    }))
}
