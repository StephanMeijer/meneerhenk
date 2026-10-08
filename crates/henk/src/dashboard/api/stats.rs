//! What happened per day, for the overview's tiles (#225): runs started,
//! of them finished and failed, findings posted and drafts written. Days
//! are UTC days.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_store::DayCounts;
use serde::Deserialize;
use time::{Date, Duration, OffsetDateTime, Time};

use super::types::{DayStats, OverviewStats};
use super::{ApiError, ApiQuery, ApiResult};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;

/// The days a request may ask for.
const MOST_DAYS: u16 = 90;
/// The days the overview shows.
const DEFAULT_DAYS: u16 = 14;

/// The query of `GET /stats/overview`.
#[derive(Debug, Deserialize)]
pub struct StatsQuery {
    /// How many days, today included: 1 to 90, 14 when left out.
    days: Option<u16>,
}

/// `GET /stats/overview`.
pub async fn overview(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<StatsQuery>,
) -> ApiResult<OverviewStats> {
    let days = query.days.unwrap_or(DEFAULT_DAYS);
    if days == 0 || days > MOST_DAYS {
        return Err(ApiError::bad_request(format!("days is 1 to {MOST_DAYS}.")));
    }
    let to = OffsetDateTime::now_utc().date();
    let from = to - Duration::days(i64::from(days) - 1);
    let since = from.with_time(Time::MIDNIGHT).assume_utc();
    let counts = dashboard.app.store.daily_stats(since).await?;
    Ok(Json(OverviewStats {
        from: from.to_string(),
        to: to.to_string(),
        days: every_day(from, to, &counts),
    }))
}

/// Every day from `from` to `to`, with what `counts` says of it and zeros
/// for a day it does not mention.
fn every_day(from: Date, to: Date, counts: &[DayCounts]) -> Vec<DayStats> {
    let mut days = Vec::new();
    let mut day = from;
    while day <= to {
        let name = day.to_string();
        let counted = counts.iter().find(|c| c.day == name);
        days.push(DayStats {
            runs: counted.map_or(0, |c| c.runs),
            finished: counted.map_or(0, |c| c.finished),
            failed: counted.map_or(0, |c| c.failed),
            findings_posted: counted.map_or(0, |c| c.findings_posted),
            drafts: counted.map_or(0, |c| c.drafts),
            day: name,
        });
        let Some(next) = day.next_day() else { break };
        day = next;
    }
    days
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use time::Month;

    use super::*;

    #[test]
    fn every_day_is_there_and_a_quiet_one_is_zeros() {
        let from = Date::from_calendar_date(2026, Month::September, 29).unwrap();
        let to = Date::from_calendar_date(2026, Month::October, 2).unwrap();
        let counts = vec![DayCounts {
            day: "2026-09-30".into(),
            runs: 4,
            finished: 3,
            failed: 1,
            findings_posted: 2,
            drafts: 7,
        }];
        let days = every_day(from, to, &counts);
        let names: Vec<_> = days.iter().map(|d| d.day.as_str()).collect();
        assert_eq!(
            names,
            ["2026-09-29", "2026-09-30", "2026-10-01", "2026-10-02"]
        );
        assert_eq!((days[1].runs, days[1].failed, days[1].drafts), (4, 1, 7));
        assert_eq!((days[0].runs, days[3].findings_posted), (0, 0));
    }
}
