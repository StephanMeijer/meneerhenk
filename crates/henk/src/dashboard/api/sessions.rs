//! Session reliability (#229): how each session of the recent reviews
//! ended, the review loop's reviewer and executor, and the lanes and
//! fact-checks of reviews from before the loop (#310), each with its role. A session that
//! keeps dropping out makes reviews thinner; this shows it without opening
//! runs one by one.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_store::{MOST_SESSION_REVIEWS, SessionEnding, SessionStatus};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::types::{ReviewMark, SessionOutcome, SessionReasons, SessionRow, SessionStats};
use super::{ApiError, ApiQuery, ApiResult};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;

/// The reviews the overview shows.
const DEFAULT_LAST: u32 = 30;
/// The most reviews `last` may ask for.
const MOST_LAST: u32 = 200;

/// What a model endpoint's rate limit reads as in a session's error, also
/// when the retries ran out on it: `LlmError::RateLimited`'s text.
const RATE_LIMITED: &str = "rate limited by the model endpoint";

/// The query of `GET /stats/sessions`: the newest `last` reviews, or every
/// review since `since` (the newest [`MOST_SESSION_REVIEWS`] of them).
#[derive(Debug, Deserialize)]
pub struct SessionsQuery {
    last: Option<u32>,
    since: Option<String>,
}

/// `GET /stats/sessions`.
pub async fn sessions(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<SessionsQuery>,
) -> ApiResult<SessionStats> {
    let (since, reviews) = match (query.last, query.since.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request("Ask for last or since, not both."));
        }
        (None, Some(since)) => (Some(read_since(since)?), MOST_SESSION_REVIEWS),
        (last, None) => {
            let last = last.unwrap_or(DEFAULT_LAST);
            if last == 0 || last > MOST_LAST {
                return Err(ApiError::bad_request(format!("last is 1 to {MOST_LAST}.")));
            }
            (None, last)
        }
    };
    let endings = dashboard.app.store.session_endings(since, reviews).await?;
    Ok(Json(session_stats(&endings)))
}

fn read_since(value: &str) -> Result<OffsetDateTime, ApiError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| {
        ApiError::bad_request("since is not an RFC 3339 time such as 2026-10-07T12:00:00Z.")
    })
}

/// The grid: the reviews oldest first, and per session its outcome in each,
/// `None` where it did not run. `endings` come newest review first.
pub(super) fn session_stats(endings: &[SessionEnding]) -> SessionStats {
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

    let mut rows: BTreeMap<&str, SessionRow> = BTreeMap::new();
    for ending in endings {
        let name = row_name(&ending.name);
        let row = rows.entry(name).or_insert_with(|| SessionRow {
            name: name.to_owned(),
            role: ending.role.as_str().to_owned(),
            models: Vec::new(),
            outcomes: vec![None; reviews.len()],
            ran: 0,
            finished: 0,
            timed_out: 0,
            did_not_finish: 0,
            reasons: SessionReasons::default(),
        });
        tally(row, ending);
        if let Some(slot) = column
            .get(ending.run_id.as_str())
            .and_then(|i| row.outcomes.get_mut(*i))
        {
            *slot = Some(SessionOutcome {
                run_id: ending.run_id.as_str().to_owned(),
                model: ending.model.clone(),
                status: ending.status.as_str().to_owned(),
                reason: reason_of(ending.status, ending.error.as_deref()).map(str::to_owned),
            });
        }
    }
    let mut sessions: Vec<SessionRow> = rows.into_values().collect();
    sessions.sort_by_key(|row| order(&row.name));
    SessionStats { reviews, sessions }
}

/// Counts one ending into its session's row; `models` stays newest first.
fn tally(row: &mut SessionRow, ending: &SessionEnding) {
    if !row.models.contains(&ending.model) {
        row.models.push(ending.model.clone());
    }
    row.ran += 1;
    match ending.status {
        SessionStatus::Finished => row.finished += 1,
        SessionStatus::TimedOut => row.timed_out += 1,
        SessionStatus::DidNotFinish => row.did_not_finish += 1,
        SessionStatus::Running => {}
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

/// The row a session goes in: the `fixer` of a review before #323 is the
/// executor's, so one row spans old and new reviews.
fn row_name(session: &str) -> &str {
    if session == "fixer" {
        "executor"
    } else {
        session
    }
}

/// The reviewer, then the executor; then lanes, by name; then the
/// fact-check sessions, by number.
fn order(name: &str) -> (u8, u32, String) {
    match (name, name.strip_prefix("check-")) {
        ("reviewer", _) => (0, 0, String::new()),
        ("executor", _) => (1, 0, String::new()),
        (_, Some(number)) => (3, number.parse().unwrap_or(u32::MAX), name.to_owned()),
        (_, None) => (2, 0, name.to_owned()),
    }
}

/// Why a session did not finish, from how it ended: `time_limit`,
/// `cancelled`, `rate_limit`, `declined`, `stuck` or `provider_error`.
/// `None` for a session that finished or still runs. The texts are the ones
/// henk-session stores for each way a session stops.
pub(super) fn reason_of(status: SessionStatus, error: Option<&str>) -> Option<&'static str> {
    match status {
        SessionStatus::Finished | SessionStatus::Running => None,
        SessionStatus::TimedOut => Some("time_limit"),
        SessionStatus::DidNotFinish => Some(match error.unwrap_or_default() {
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
    use henk_store::SessionRole;

    use super::*;

    fn ending(
        run: &str,
        name: &str,
        model: &str,
        status: SessionStatus,
        error: Option<&str>,
    ) -> SessionEnding {
        // The roles the migration gave sessions of older reviews (#324).
        let role = match name {
            "reviewer" => SessionRole::Reviewer,
            "executor" | "fixer" => SessionRole::Executor,
            _ if name.starts_with("check-") => SessionRole::Check,
            _ => SessionRole::Lane,
        };
        SessionEnding {
            run_id: RunId::parse(run).unwrap(),
            started_at: format!("2026-10-0{}T12:00:00Z", &run[2..]),
            name: name.into(),
            role,
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
            (SessionStatus::Finished, None, None),
            (SessionStatus::Running, None, None),
            (SessionStatus::TimedOut, None, Some("time_limit")),
            (
                SessionStatus::DidNotFinish,
                Some("cancelled".to_owned()),
                Some("cancelled"),
            ),
            (
                SessionStatus::DidNotFinish,
                Some(rate.to_string()),
                Some("rate_limit"),
            ),
            (
                SessionStatus::DidNotFinish,
                Some(gave_up.to_string()),
                Some("rate_limit"),
            ),
            (
                SessionStatus::DidNotFinish,
                Some(LlmError::Overloaded.to_string()),
                Some("provider_error"),
            ),
            (
                SessionStatus::DidNotFinish,
                Some(server.to_string()),
                Some("provider_error"),
            ),
            (
                SessionStatus::DidNotFinish,
                Some(format!("the model declined ({})", "policy")),
                Some("declined"),
            ),
            (
                SessionStatus::DidNotFinish,
                Some(format!("stuck repeating {}", "read_file")),
                Some("stuck"),
            ),
            (SessionStatus::DidNotFinish, None, Some("provider_error")),
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
    fn the_grid_lines_sessions_up_by_review_with_each_square_s_own_model() {
        // Newest review first, as the store gives them. lane-b was added
        // in r-2; lane-a moved from m-old to m-new; check-10 sorts after
        // check-2.
        let endings = [
            ending("r-3", "check-10", "c", SessionStatus::Finished, None),
            ending("r-3", "check-2", "c", SessionStatus::TimedOut, None),
            ending(
                "r-3",
                "lane-a",
                "m-new",
                SessionStatus::DidNotFinish,
                Some("cancelled"),
            ),
            ending("r-3", "lane-b", "m-b", SessionStatus::Finished, None),
            ending("r-2", "lane-a", "m-new", SessionStatus::Finished, None),
            ending(
                "r-2",
                "lane-b",
                "m-b",
                SessionStatus::DidNotFinish,
                Some("stuck repeating x"),
            ),
            ending("r-1", "lane-a", "m-old", SessionStatus::Finished, None),
        ];
        let stats = session_stats(&endings);
        let reviews: Vec<_> = stats.reviews.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(reviews, ["r-1", "r-2", "r-3"], "oldest first");
        let names: Vec<_> = stats.sessions.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["lane-a", "lane-b", "check-2", "check-10"]);

        let a = &stats.sessions[0];
        assert_eq!(a.role, "lane");
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

        let b = &stats.sessions[1];
        assert!(b.outcomes[0].is_none(), "lane-b did not run in r-1");
        assert_eq!((b.ran, b.reasons.stuck), (2, 1));

        let check = &stats.sessions[2];
        assert_eq!(check.role, "check");
        assert_eq!((check.timed_out, check.reasons.time_limit), (1, 1));
        assert_eq!(check.outcomes.len(), 3);
    }

    #[test]
    fn a_review_loops_reviewer_and_executor_come_first_with_their_roles() {
        // r-3 ran the loop; r-2 too, before #323 named its executor fixer;
        // r-1 ran lanes before the loop (#310).
        let endings = [
            ending("r-3", "executor", "m-e", SessionStatus::TimedOut, None),
            ending("r-3", "reviewer", "m-r", SessionStatus::Finished, None),
            ending("r-2", "fixer", "m-f", SessionStatus::Finished, None),
            ending("r-2", "reviewer", "m-r", SessionStatus::Finished, None),
            ending("r-1", "check-1", "c", SessionStatus::Finished, None),
            ending("r-1", "lane-a", "m-a", SessionStatus::Finished, None),
        ];
        let stats = session_stats(&endings);
        let rows: Vec<_> = stats
            .sessions
            .iter()
            .map(|l| (l.name.as_str(), l.role.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("reviewer", "reviewer"),
                ("executor", "executor"),
                ("lane-a", "lane"),
                ("check-1", "check"),
            ]
        );
        assert!(
            stats.sessions[0].outcomes[0].is_none(),
            "no reviewer in r-1"
        );
        let executor = &stats.sessions[1];
        assert_eq!(executor.ran, 2, "the fixer of r-2 is in the executor's row");
        assert_eq!(executor.models, ["m-e", "m-f"]);
        assert_eq!(executor.reasons.time_limit, 1);
    }

    #[test]
    fn no_reviews_is_an_empty_grid() {
        let stats = session_stats(&[]);
        assert!(stats.reviews.is_empty() && stats.sessions.is_empty());
    }
}
