//! Runs: listed, one in full, its timeline, tool calls and transcripts;
//! started and cancelled.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use henk_domain::run::RunId;
use henk_events::EventSource;
use henk_store::{RunFilter, RunKey, RunRecord, ToolUsage};
use serde::Deserialize;
use tracing::info;

use super::types::{
    Cancelled, Draft, EventSummary, Finding, Lane, Page, RunDetail, RunEvent, RunSummary,
    StartRequest, Started, ToolCall, ToolUsageRow, Transcript, TranscriptRef,
};
use super::{ApiError, ApiQuery, ApiResult, cursor, limit, read_cursor, read_time};
use crate::dashboard::Dashboard;
use crate::dashboard::actions::{record_cancel, requester};
use crate::dashboard::auth::{ApiAct, ApiViewer};
use crate::dashboard::views::{KINDS, PLATFORMS, STATUSES};
use crate::hooks::publish;
use crate::hooks::requests::{Start, start_event};
use crate::runs::TranscriptView;

/// What `GET /runs` takes. Every filter is optional.
#[derive(Debug, Default, Deserialize)]
pub struct RunQuery {
    kind: Option<String>,
    status: Option<String>,
    platform: Option<String>,
    repo: Option<String>,
    target: Option<u64>,
    since: Option<String>,
    until: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

/// A named value from one of the dashboard's tables, or why it is not one.
fn named<T: Copy>(
    table: &[(&str, T)],
    field: &str,
    value: Option<&str>,
) -> Result<Option<T>, ApiError> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    table
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, v)| Some(*v))
        .ok_or_else(|| {
            let names: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
            ApiError::bad_request(format!("{field} is one of {}.", names.join(", ")))
        })
}

impl RunQuery {
    fn filter(&self) -> Result<RunFilter, ApiError> {
        let before = self
            .cursor
            .as_deref()
            .map(read_cursor)
            .transpose()?
            .map(|(started_at, id)| RunKey { started_at, id });
        Ok(RunFilter {
            kind: named(KINDS, "kind", self.kind.as_deref())?,
            status: named(STATUSES, "status", self.status.as_deref())?,
            platform: named(PLATFORMS, "platform", self.platform.as_deref())?,
            repo: self
                .repo
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_owned),
            target: self.target,
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

/// `GET /runs`: runs newest first, by filter, a page at a time.
pub async fn list(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<RunQuery>,
) -> ApiResult<Page<RunSummary>> {
    let filter = query.filter()?;
    let limit = limit(query.limit);
    let runs = dashboard
        .app
        .store
        .list_runs(&filter, henk_store::Page::new(limit, 0))
        .await?;
    let next = (u32::try_from(runs.len()).ok() == Some(limit))
        .then(|| runs.last().map(|r| cursor(&r.started_at, r.id.as_str())))
        .flatten();
    let settings = &dashboard.app.settings;
    Ok(Json(Page {
        items: runs
            .iter()
            .map(|run| RunSummary::from_record(settings, run))
            .collect(),
        next,
    }))
}

/// The run a path names, or a 400 or 404.
async fn run_of(dashboard: &Dashboard, id: String) -> Result<RunRecord, ApiError> {
    let id = RunId::parse(id).map_err(|_| ApiError::bad_request("That is not a run id."))?;
    dashboard
        .app
        .store
        .run(&id)
        .await?
        .ok_or_else(|| ApiError::not_found("No such run."))
}

/// `GET /runs/{id}`: one run with its lanes, findings, drafts, transcripts,
/// tool usage, timeline and requests.
pub async fn detail(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path(id): Path<String>,
) -> ApiResult<RunDetail> {
    let run = run_of(&dashboard, id).await?;
    let store = &dashboard.app.store;
    let calls = store.tool_calls(&run.id).await?;
    Ok(Json(RunDetail {
        summary: run.summary.clone(),
        error: run.error.clone(),
        check_id: run.check_id.clone(),
        heartbeat_at: run.heartbeat_at.clone(),
        lanes: store.lanes(&run.id).await?.iter().map(Lane::from).collect(),
        findings: store
            .findings(&run.id)
            .await?
            .iter()
            .map(Finding::from)
            .collect(),
        drafts: store
            .drafts(&run.id)
            .await?
            .iter()
            .map(Draft::from)
            .collect(),
        transcripts: store
            .transcripts(&run.id)
            .await?
            .iter()
            .map(TranscriptRef::from)
            .collect(),
        tool_usage: ToolUsage::from_calls(&calls)
            .iter()
            .map(ToolUsageRow::from)
            .collect(),
        events: store
            .events(&run.id)
            .await?
            .iter()
            .map(RunEvent::from)
            .collect(),
        requests: store
            .inbound_events_for_run(&run.id)
            .await?
            .iter()
            .map(EventSummary::from)
            .collect(),
        run: RunSummary::from_record(&dashboard.app.settings, &run),
    }))
}

/// What `GET /runs/{id}/events` takes.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    limit: Option<u32>,
    cursor: Option<String>,
}

/// `GET /runs/{id}/events`: the run's timeline in order, a page at a time.
/// The cursor is the number of lines already read.
pub async fn events(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<EventsQuery>,
) -> ApiResult<Page<RunEvent>> {
    let run = run_of(&dashboard, id).await?;
    let from = match query.cursor.as_deref() {
        None => 0,
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| ApiError::bad_request("The cursor is not one this API gave out."))?,
    };
    let limit = usize::try_from(limit(query.limit)).unwrap_or(usize::MAX);
    let events = dashboard.app.store.events(&run.id).await?;
    let items: Vec<RunEvent> = events
        .iter()
        .skip(from)
        .take(limit)
        .map(RunEvent::from)
        .collect();
    let read = from.saturating_add(items.len());
    let next = (read < events.len()).then(|| read.to_string());
    Ok(Json(Page { items, next }))
}

/// What `GET /runs/{id}/tool-calls` takes.
#[derive(Debug, Default, Deserialize)]
pub struct ToolCallQuery {
    session: Option<String>,
    tool: Option<String>,
    outcome: Option<String>,
}

/// `GET /runs/{id}/tool-calls`: the run's tool calls in order, by session,
/// tool and outcome.
pub async fn tool_calls(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ToolCallQuery>,
) -> ApiResult<Vec<ToolCall>> {
    let run = run_of(&dashboard, id).await?;
    let matches = |wanted: Option<&String>, value: &str| wanted.is_none_or(|w| w == value);
    Ok(Json(
        dashboard
            .app
            .store
            .tool_calls(&run.id)
            .await?
            .iter()
            .filter(|c| {
                matches(query.session.as_ref(), &c.session)
                    && matches(query.tool.as_ref(), &c.tool)
                    && matches(query.outcome.as_ref(), &c.outcome)
            })
            .map(ToolCall::from)
            .collect(),
    ))
}

/// `GET /runs/{id}/transcripts/{session}`: one session's conversation.
pub async fn transcript(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path((id, session)): Path<(String, String)>,
) -> ApiResult<Transcript> {
    let run = run_of(&dashboard, id).await?;
    let record = dashboard
        .app
        .store
        .transcript(&run.id, &session)
        .await?
        .ok_or_else(|| ApiError::not_found("That run kept no conversation for that session."))?;
    let view = TranscriptView::parse(&record.body).map_err(|error| {
        tracing::warn!(%error, run = %run.id, session, "a stored transcript is not JSON");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store",
            "The stored conversation could not be read.",
        )
    })?;
    Ok(Json(Transcript::from(view)))
}

/// `POST /runs`: starts a review, plan or address run exactly as the
/// dashboard's start form does. The request becomes an event, and the
/// listeners apply the allowlist and every refusal; the event's outcomes
/// say what came of it.
pub async fn start(
    State(dashboard): State<Arc<Dashboard>>,
    act: ApiAct<StartRequest>,
) -> Result<Response, ApiError> {
    let ApiAct { session, body } = act;
    let kind = Start::parse(&body.kind)
        .ok_or_else(|| ApiError::bad_request("kind is one of review, plan, address."))?;
    let given =
        |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let who = requester(session.github_id);
    let commit = given(body.commit);
    let event = start_event(
        kind,
        body.url.trim(),
        commit.as_deref(),
        given(body.note),
        EventSource::Dashboard {
            requester: who.clone(),
        },
        Some(who.clone()),
    )
    .map_err(|message| ApiError::bad_request(format!("{message}.")))?;
    let event_id = publish(&dashboard.bus, event);
    info!(event = %event_id, requester = %who, "started through the API");
    Ok((StatusCode::ACCEPTED, Json(Started { event_id })).into_response())
}

/// `POST /runs/{id}/cancel`: cancels a running run, as the dashboard's
/// cancel button does. The request is recorded as an event either way.
pub async fn cancel(
    State(dashboard): State<Arc<Dashboard>>,
    Path(id): Path<String>,
    act: ApiAct<serde_json::Value>,
) -> Result<Response, ApiError> {
    let run = RunId::parse(id).map_err(|_| ApiError::bad_request("That is not a run id."))?;
    let who = requester(act.session.github_id);
    let cancelled = dashboard.coordinator.cancel(&run, who.clone());
    record_cancel(&dashboard, &run, &who, cancelled).await;
    if cancelled {
        info!(run = %run, requester = %who, "cancelled through the API");
        Ok((
            StatusCode::ACCEPTED,
            Json(Cancelled {
                run_id: run.as_str().to_owned(),
            }),
        )
            .into_response())
    } else {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "That run is not running here: it ended, or another Henk process runs it.",
        ))
    }
}
