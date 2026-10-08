//! Review quality (#205): what became of the lanes' drafts across runs,
//! per model, lane, repository or pull request, and the drafts behind the
//! numbers. The rejection rate is rejected of judged, where judged is what
//! a checker decided: confirmed, rejected and repeats.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_store::{
    DayRates, DraftFilter, DraftGroup, DraftKey, DraftRates, DraftVerdict, VerdictFilter,
};
use serde::Deserialize;
use time::{Date, Duration, OffsetDateTime};

use super::types::{
    DayRate, Draft, DraftCount, DraftItem, Page, QualityRow, QualitySeries, link_to,
};
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

/// The groups the rejection-rate chart draws: the largest of the period.
const CHART_GROUPS: usize = 6;
/// How far back the chart goes when the period is all time.
const CHART_DAYS: i64 = 90;

/// `GET /quality/daily`: the rejection rate per UTC day of the largest
/// groups (#228), every day of the period, a day the check judged nothing
/// `null`.
pub async fn daily(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<QualityQuery>,
) -> ApiResult<Vec<QualitySeries>> {
    let group = query.group()?;
    let filter = DraftFilter {
        verdict: None,
        before: None,
        ..query.filter()?
    };
    let store = &dashboard.app.store;
    let largest: Vec<String> = store
        .draft_rates(group, &filter)
        .await?
        .into_iter()
        .take(CHART_GROUPS)
        .map(|rates| rates.key)
        .collect();
    let rows = store.daily_draft_rates(group, &filter).await?;
    let today = OffsetDateTime::now_utc().date();
    let from = first_day(filter.since.as_deref(), &rows, today);
    Ok(Json(series(&largest, &rows, from, today)))
}

/// The first day the chart shows when it ends `today`: the day of `since`,
/// or of the earliest row when there is none, but no more than
/// [`CHART_DAYS`] back.
fn first_day(since: Option<&str>, rows: &[DayRates], today: Date) -> Date {
    let earliest = today - Duration::days(CHART_DAYS - 1);
    since
        .and_then(|since| {
            OffsetDateTime::parse(since, &time::format_description::well_known::Rfc3339).ok()
        })
        .map(OffsetDateTime::date)
        .or_else(|| rows.iter().filter_map(|r| day_of(&r.day)).min())
        .map_or(today, |from| from.max(earliest))
}

fn day_of(text: &str) -> Option<Date> {
    Date::parse(
        text,
        time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()
}

/// Each of `keys` over every day from `from` to `to`.
fn series(keys: &[String], rows: &[DayRates], from: Date, to: Date) -> Vec<QualitySeries> {
    let days = super::stats::dates(from, to);
    keys.iter()
        .map(|key| QualitySeries {
            key: key.clone(),
            days: days
                .iter()
                .map(|date| {
                    let day = date.to_string();
                    let found = rows.iter().find(|r| &r.key == key && r.day == day);
                    let (judged, rejected) = found.map_or((0, 0), |r| (r.judged, r.rejected));
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "counts of drafts are far below 2^52"
                    )]
                    let rate = (judged > 0).then(|| rejected as f64 / judged as f64);
                    DayRate {
                        day,
                        judged,
                        rejected,
                        rate,
                    }
                })
                .collect(),
        })
        .collect()
}

/// `GET /drafts/count`: how many drafts the filters of `GET /drafts`
/// match, over every page.
pub async fn count(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<QualityQuery>,
) -> ApiResult<DraftCount> {
    let filter = DraftFilter {
        before: None,
        ..query.filter()?
    };
    let count = dashboard.app.store.count_drafts(&filter).await?;
    Ok(Json(DraftCount { count }))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::float_cmp)]

    use time::Month;

    use super::*;

    #[test]
    fn a_series_has_every_day_and_no_rate_on_a_day_nothing_was_judged() {
        let from = Date::from_calendar_date(2026, Month::October, 5).unwrap();
        let to = Date::from_calendar_date(2026, Month::October, 7).unwrap();
        let rows = vec![
            DayRates {
                key: "m".into(),
                day: "2026-10-05".into(),
                judged: 4,
                rejected: 1,
            },
            DayRates {
                key: "m".into(),
                day: "2026-10-07".into(),
                judged: 0,
                rejected: 0,
            },
            DayRates {
                key: "other".into(),
                day: "2026-10-06".into(),
                judged: 2,
                rejected: 2,
            },
        ];
        let got = series(&["m".to_owned()], &rows, from, to);
        assert_eq!(got.len(), 1);
        let days = &got[0].days;
        assert_eq!(
            days.iter().map(|d| d.day.as_str()).collect::<Vec<_>>(),
            ["2026-10-05", "2026-10-06", "2026-10-07"]
        );
        assert_eq!(days[0].rate, Some(0.25));
        assert_eq!(days[1].rate, None, "a day without drafts");
        assert_eq!(days[2].rate, None, "a day with drafts, none judged");
    }

    #[test]
    fn the_chart_starts_at_since_or_the_first_row_and_at_most_ninety_days_back() {
        let day = |month, day| Date::from_calendar_date(2026, month, day).unwrap();
        let today = day(Month::October, 8);
        let rows = vec![DayRates {
            key: "m".into(),
            day: "2026-10-05".into(),
            judged: 1,
            rejected: 0,
        }];
        assert_eq!(first_day(None, &[], today), today, "nothing yet");
        assert_eq!(first_day(None, &rows, today), day(Month::October, 5));
        assert_eq!(
            first_day(Some("2026-10-07T10:06:00Z"), &rows, today),
            day(Month::October, 7),
            "since wins over the rows"
        );
        let ninety_days = day(Month::July, 11);
        assert_eq!(today - ninety_days, Duration::days(CHART_DAYS - 1));
        assert_eq!(
            first_day(Some("2020-01-01T00:00:00Z"), &rows, today),
            ninety_days
        );
        let old = vec![DayRates {
            day: "2025-01-01".into(),
            ..rows[0].clone()
        }];
        assert_eq!(first_day(None, &old, today), ninety_days);
    }
}
