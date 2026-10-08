//! Lane reliability (#229): how each lane and fact-check session of the
//! recent reviews ended. A review stands on the lanes that finished
//! (§3.2), so a lane that keeps dropping out makes reviews thinner; this
//! shows it without opening runs one by one.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_store::{LaneEnding, LaneStatus, MOST_LANE_REVIEWS, session_kind};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::types::{LaneOutcome, LaneReasons, LaneRow, LaneStats, ReviewMark};
use super::{ApiError, ApiQuery, ApiResult};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;

/// The reviews the overview shows.
const DEFAULT_LAST: u32 = 30;
/// The most reviews `last` may ask for.
const MOST_LAST: u32 = 200;

/// What a model endpoint's rate limit reads as in a lane's error, also
/// when the retries ran out on it: `LlmError::RateLimited`'s text.
const RATE_LIMITED: &str = "rate limited by the model endpoint";

/// The query of `GET /stats/lanes`: the newest `last` reviews, or every
/// review since `since` (the newest [`MOST_LANE_REVIEWS`] of them).
#[derive(Debug, Deserialize)]
pub struct LanesQuery {
    last: Option<u32>,
    since: Option<String>,
}

/// `GET /stats/lanes`.
pub async fn lanes(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<LanesQuery>,
) -> ApiResult<LaneStats> {
    let (since, reviews) = match (query.last, query.since.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request("Ask for last or since, not both."));
        }
        (None, Some(since)) => (Some(read_since(since)?), MOST_LANE_REVIEWS),
        (last, None) => {
            let last = last.unwrap_or(DEFAULT_LAST);
            if last == 0 || last > MOST_LAST {
                return Err(ApiError::bad_request(format!("last is 1 to {MOST_LAST}.")));
            }
            (None, last)
        }
    };
    let endings = dashboard.app.store.lane_endings(since, reviews).await?;
    Ok(Json(lane_stats(&endings)))
}

fn read_since(value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
        ApiError::bad_request("since is not an RFC 3339 time such as 2026-10-07T12:00:00Z.")
    })
}

/// The grid: the reviews oldest first, and per lane its outcome in each,
/// `None` where it did not run. `endings` come newest review first.
pub(super) fn lane_stats(endings: &[LaneEnding]) -> LaneStats {
    let mut reviews: Vec<ReviewMark> = Vec::new();
    for ending in endings {
        if reviews
            .last()
            .is_none_or(|r| r.run_id != ending.run_id.as_str())
        {
            reviews.push(ReviewMark {
                run_id: ending.run_id.as_str().to_owned(),
                started_at: ending.started_at.clone(),
            });
        }
    }
    reviews.reverse();
    let column: BTreeMap<&str, usize> = reviews
        .iter()
        .enumerate()
        .map(|(i, r)| (r.run_id.as_str(), i))
        .collect();

    let mut rows: BTreeMap<&str, LaneRow> = BTreeMap::new();
    for ending in endings {
        let row = rows.entry(&ending.name).or_insert_with(|| LaneRow {
            name: ending.name.clone(),
            kind: session_kind(&ending.name).to_owned(),
            models: Vec::new(),
            outcomes: vec![None; reviews.len()],
            ran: 0,
            finished: 0,
            timed_out: 0,
            did_not_finish: 0,
            reasons: LaneReasons::default(),
        });
        tally(row, ending);
        if let Some(slot) = column
            .get(ending.run_id.as_str())
            .and_then(|i| row.outcomes.get_mut(*i))
        {
            *slot = Some(LaneOutcome {
                run_id: ending.run_id.as_str().to_owned(),
                model: ending.model.clone(),
                status: ending.status.as_str().to_owned(),
                reason: reason_of(ending.status, ending.error.as_deref()).map(str::to_owned),
            });
        }
    }
    let mut lanes: Vec<LaneRow> = rows.into_values().collect();
    lanes.sort_by_key(|row| order(&row.name));
    LaneStats { reviews, lanes }
}

/// Counts one ending into its lane's row; `models` stays newest first.
fn tally(row: &mut LaneRow, ending: &LaneEnding) {
    if !row.models.contains(&ending.model) {
        row.models.push(ending.model.clone());
    }
    row.ran += 1;
    match ending.status {
        LaneStatus::Finished => row.finished += 1,
        LaneStatus::TimedOut => row.timed_out += 1,
        LaneStatus::DidNotFinish => row.did_not_finish += 1,
        LaneStatus::Running => {}
    }
    let reasons = &mut row.reasons;
    match reason_of(ending.status, ending.error.as_deref()) {
        Some("time_limit") => reasons.time_limit += 1,
        Some("rate_limit") => reasons.rate_limit += 1,
        Some("cancelled") => reasons.cancelled += 1,
        Some("declined") => reasons.declined += 1,
        Some("stuck") => reasons.stuck += 1,
        Some(_) => reasons.provider_error += 1,
        None => {}
    }
}

/// Lanes first, by name; then the fact-check sessions, by number.
fn order(name: &str) -> (bool, u32, String) {
    match name.strip_prefix("check-") {
        Some(number) => (true, number.parse().unwrap_or(u32::MAX), name.to_owned()),
        None => (false, 0, name.to_owned()),
    }
}

/// Why a lane did not finish, from how it ended: `time_limit`,
/// `cancelled`, `rate_limit`, `declined`, `stuck` or `provider_error`.
/// `None` for a lane that finished or still runs. The texts are the ones
/// henk-session stores for each way a session stops.
pub(super) fn reason_of(status: LaneStatus, error: Option<&str>) -> Option<&'static str> {
    match status {
        LaneStatus::Finished | LaneStatus::Running => None,
        LaneStatus::TimedOut => Some("time_limit"),
        LaneStatus::DidNotFinish => Some(match error.unwrap_or_default() {
            "cancelled" => "cancelled",
            text if text.contains(RATE_LIMITED) => "rate_limit",
            text if text.starts_with("the model declined") => "declined",
            text if text.starts_with("stuck repeating") => "stuck",
            _ => "provider_error",
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use henk_domain::run::RunId;
    use henk_llm::LlmError;

    use super::*;

    fn ending(
        run: &str,
        name: &str,
        model: &str,
        status: LaneStatus,
        error: Option<&str>,
    ) -> LaneEnding {
        LaneEnding {
            run_id: RunId::parse(run).unwrap(),
            started_at: format!("2026-10-0{}T12:00:00Z", &run[2..]),
            name: name.into(),
            model: model.into(),
            status,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn every_way_a_session_stops_has_its_reason() {
        let rate = LlmError::RateLimited { retry_after: None };
        let gave_up = LlmError::RetriesExhausted {
            attempts: 4,
            last: Box::new(LlmError::RateLimited { retry_after: None }),
        };
        let server = LlmError::Server {
            status: 502,
            body: "bad gateway".into(),
        };
        let cases = [
            (LaneStatus::Finished, None, None),
            (LaneStatus::Running, None, None),
            (LaneStatus::TimedOut, None, Some("time_limit")),
            (
                LaneStatus::DidNotFinish,
                Some("cancelled".to_owned()),
                Some("cancelled"),
            ),
            (
                LaneStatus::DidNotFinish,
                Some(rate.to_string()),
                Some("rate_limit"),
            ),
            (
                LaneStatus::DidNotFinish,
                Some(gave_up.to_string()),
                Some("rate_limit"),
            ),
            (
                LaneStatus::DidNotFinish,
                Some(LlmError::Overloaded.to_string()),
                Some("provider_error"),
            ),
            (
                LaneStatus::DidNotFinish,
                Some(server.to_string()),
                Some("provider_error"),
            ),
            (
                LaneStatus::DidNotFinish,
                Some(format!("the model declined ({})", "policy")),
                Some("declined"),
            ),
            (
                LaneStatus::DidNotFinish,
                Some(format!("stuck repeating {}", "read_file")),
                Some("stuck"),
            ),
            (LaneStatus::DidNotFinish, None, Some("provider_error")),
        ];
        for (status, error, reason) in cases {
            assert_eq!(
                reason_of(status, error.as_deref()),
                reason,
                "{status:?} {error:?}"
            );
        }
    }

    #[test]
    fn the_grid_lines_lanes_up_by_review_with_each_square_s_own_model() {
        // Newest review first, as the store gives them. lane-b was added
        // in r-2; lane-a moved from m-old to m-new; check-10 sorts after
        // check-2.
        let endings = [
            ending("r-3", "check-10", "c", LaneStatus::Finished, None),
            ending("r-3", "check-2", "c", LaneStatus::TimedOut, None),
            ending(
                "r-3",
                "lane-a",
                "m-new",
                LaneStatus::DidNotFinish,
                Some("cancelled"),
            ),
            ending("r-3", "lane-b", "m-b", LaneStatus::Finished, None),
            ending("r-2", "lane-a", "m-new", LaneStatus::Finished, None),
            ending(
                "r-2",
                "lane-b",
                "m-b",
                LaneStatus::DidNotFinish,
                Some("stuck repeating x"),
            ),
            ending("r-1", "lane-a", "m-old", LaneStatus::Finished, None),
        ];
        let stats = lane_stats(&endings);
        let reviews: Vec<_> = stats.reviews.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(reviews, ["r-1", "r-2", "r-3"], "oldest first");
        let names: Vec<_> = stats.lanes.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["lane-a", "lane-b", "check-2", "check-10"]);

        let a = &stats.lanes[0];
        assert_eq!(a.kind, "lane");
        assert_eq!(a.models, ["m-new", "m-old"], "newest first");
        let models: Vec<_> = a
            .outcomes
            .iter()
            .map(|o| o.as_ref().unwrap().model.as_str())
            .collect();
        assert_eq!(models, ["m-old", "m-new", "m-new"]);
        assert_eq!(
            (a.ran, a.finished, a.did_not_finish, a.reasons.cancelled),
            (3, 2, 1, 1)
        );
        assert_eq!(
            a.outcomes[2].as_ref().unwrap().reason.as_deref(),
            Some("cancelled")
        );

        let b = &stats.lanes[1];
        assert!(b.outcomes[0].is_none(), "lane-b did not run in r-1");
        assert_eq!((b.ran, b.reasons.stuck), (2, 1));

        let check = &stats.lanes[2];
        assert_eq!(check.kind, "check");
        assert_eq!((check.timed_out, check.reasons.time_limit), (1, 1));
        assert_eq!(check.outcomes.len(), 3);
    }

    #[test]
    fn no_reviews_is_an_empty_grid() {
        let stats = lane_stats(&[]);
        assert!(stats.reviews.is_empty() && stats.lanes.is_empty());
    }
}
