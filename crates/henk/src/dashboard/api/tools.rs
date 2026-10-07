//! Tool calls across runs (#203): how each tool fares per model and kind
//! of session, and the calls that went wrong. An `ok` call succeeded; an
//! `error` failed in the tool; `refused_scope` and `refused_repeat` were
//! refused by a guard; anything else never ran.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_domain::run::RunKind;
use henk_store::{OutcomeFilter, ToolCallFilter, ToolCallKey, ToolUsage};
use serde::Deserialize;

use super::types::{Page, ToolCall, ToolCallItem, ToolSummaryRow, link_to};
use super::{ApiError, ApiQuery, ApiResult, cursor, limit, read_cursor, read_time};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;

/// Every outcome a tool call records.
const OUTCOMES: &[&str] = &[
    "ok",
    "error",
    "refused_scope",
    "refused_repeat",
    "unknown_tool",
    "malformed_arguments",
    "not_run",
    "cancelled",
];

/// Every kind of session.
const KINDS: &[&str] = &["lane", "check", "planner", "address"];

/// What `GET /tool-calls/summary` and `GET /tool-calls` take. Every
/// filter is optional.
#[derive(Debug, Default, Deserialize)]
pub struct ToolQuery {
    outcome: Option<String>,
    tool: Option<String>,
    model: Option<String>,
    session_kind: Option<String>,
    since: Option<String>,
    until: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

fn given(value: Option<&String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

impl ToolQuery {
    fn filter(&self) -> Result<ToolCallFilter, ApiError> {
        let outcome = match given(self.outcome.as_ref()) {
            None => None,
            Some(name) if name == "problems" => Some(OutcomeFilter::Problems),
            Some(name) if OUTCOMES.contains(&name.as_str()) => Some(OutcomeFilter::Is(name)),
            Some(_) => {
                return Err(ApiError::bad_request(format!(
                    "outcome is one of {}, problems.",
                    OUTCOMES.join(", ")
                )));
            }
        };
        let session_kind = given(self.session_kind.as_ref());
        if let Some(kind) = &session_kind
            && !KINDS.contains(&kind.as_str())
        {
            return Err(ApiError::bad_request(format!(
                "session_kind is one of {}.",
                KINDS.join(", ")
            )));
        }
        let before = match self.cursor.as_deref() {
            None => None,
            Some(text) => {
                let (at, id) = read_cursor(text)?;
                let id = id.parse().map_err(|_| {
                    ApiError::bad_request("The cursor is not one this API gave out.")
                })?;
                Some(ToolCallKey { at, id })
            }
        };
        Ok(ToolCallFilter {
            outcome,
            tool: given(self.tool.as_ref()),
            model: given(self.model.as_ref()),
            session_kind,
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
}

/// A share from 0 to 1.
fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "counts of tool calls are far below 2^52"
    )]
    let share = part as f64 / whole as f64;
    share
}

fn summary_row(usage: ToolUsage) -> ToolSummaryRow {
    let tally = usage.tally;
    ToolSummaryRow {
        tool: usage.tool,
        model: usage.model,
        session_kind: usage.session,
        calls: tally.calls,
        errors: tally.errors,
        refusals: tally.refusals,
        other: tally.other,
        total_ms: tally.total_ms,
        error_rate: share(tally.errors, tally.calls),
        refusal_rate: share(tally.refusals, tally.calls),
    }
}

/// `GET /tool-calls/summary`: per tool, model and kind of session, the
/// most calls first.
pub async fn summary(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<ToolQuery>,
) -> ApiResult<Vec<ToolSummaryRow>> {
    let filter = query.filter()?;
    let mut rows: Vec<ToolSummaryRow> = dashboard
        .app
        .store
        .tool_usage(&filter)
        .await?
        .into_iter()
        .map(summary_row)
        .collect();
    rows.sort_by(|a, b| {
        b.calls
            .cmp(&a.calls)
            .then_with(|| a.tool.cmp(&b.tool))
            .then_with(|| a.model.cmp(&b.model))
            .then_with(|| a.session_kind.cmp(&b.session_kind))
    });
    Ok(Json(rows))
}

/// `GET /tool-calls`: tool calls across runs, newest first, a page at a
/// time.
pub async fn list(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<ToolQuery>,
) -> ApiResult<Page<ToolCallItem>> {
    let filter = query.filter()?;
    let limit = limit(query.limit);
    let listed = dashboard
        .app
        .store
        .list_tool_calls(&filter, henk_store::Page::new(limit, 0))
        .await?;
    let next = (u32::try_from(listed.len()).ok() == Some(limit))
        .then(|| listed.last().map(|c| cursor(&c.call.at, &c.id.to_string())))
        .flatten();
    let settings = &dashboard.app.settings;
    Ok(Json(Page {
        items: listed
            .iter()
            .map(|listing| ToolCallItem {
                run_id: listing.run_id.clone(),
                repo: listing.repo.clone(),
                target: listing.target,
                target_url: link_to(
                    settings,
                    listing.platform,
                    &listing.repo,
                    listing.target,
                    listing.kind == RunKind::Plan,
                ),
                call: ToolCall::from(&listing.call),
                transcript_kept: listing.transcript_kept,
            })
            .collect(),
        next,
    }))
}
