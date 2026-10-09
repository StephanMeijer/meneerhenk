//! Runs: listed, one in full, its timeline, tool calls and transcripts;
//! started and cancelled.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use henk_domain::allowlist::Platform;
use henk_domain::run::{RunId, RunKind};
use henk_events::EventSource;
use henk_store::{
    InboundEvent, OutcomeRecord, RunFilter, RunKey, RunRecord, RunStatus, StoreError, ToolUsage,
};
use serde::Deserialize;
use tracing::{info, warn};

use super::types::{
    Cancelled, Draft, EventSummary, Finding, Lane, LaneDot, Page, RunCount, RunDetail, RunEvent,
    RunSummary, Stage, StageDot, StartRequest, Started, ToolCall, ToolUsageRow, Transcript,
    TranscriptRef,
};
use super::{ApiError, ApiQuery, ApiResult, cursor, limit, read_cursor, read_time};
use crate::app::App;
use crate::coordinator::Coordinator;
use crate::dashboard::Dashboard;
use crate::dashboard::auth::{ApiAct, ApiViewer};
use crate::hooks::requests::{Start, start_event};
use crate::hooks::{now_rfc3339, publish};
use crate::ids::new_event_id;
use crate::runs::TranscriptView;

/// What `GET /runs` takes. Every filter is optional. The MCP server's
/// `list_runs` fills it from its arguments (#248).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RunQuery {
    pub(crate) kind: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) platform: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) target: Option<u64>,
    pub(crate) since: Option<String>,
    pub(crate) until: Option<String>,
    pub(crate) limit: Option<u32>,
    pub(crate) cursor: Option<String>,
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
    Ok(Json(list_runs(&dashboard.app, &query).await?))
}

/// Runs newest first, by `query`, a page at a time: `GET /runs` and the
/// MCP server's `list_runs`.
pub(crate) async fn list_runs(app: &App, query: &RunQuery) -> Result<Page<RunSummary>, ApiError> {
    let filter = query.filter()?;
    let limit = limit(query.limit);
    let runs = app
        .store
        .list_runs(&filter, henk_store::Page::new(limit, 0))
        .await?;
    let next = (u32::try_from(runs.len()).ok() == Some(limit))
        .then(|| runs.last().map(|r| cursor(&r.started_at, r.id.as_str())))
        .flatten();
    Ok(Page {
        items: summaries(app, &runs).await?,
        next,
    })
}

/// Runs as lists show them: each with where its lanes and stages stand,
/// read for all of them at once (#225).
pub(super) async fn summaries(
    app: &App,
    runs: &[RunRecord],
) -> Result<Vec<RunSummary>, StoreError> {
    let ids: Vec<RunId> = runs.iter().map(|r| r.id.clone()).collect();
    let store = &app.store;
    let lanes = store.lanes_of(&ids).await?;
    let stages = store.stages_of(&ids).await?;
    let settings = &app.settings;
    Ok(runs
        .iter()
        .map(|run| {
            let mut summary = RunSummary::from_record(settings, run);
            summary.lanes = lanes
                .iter()
                .filter(|(of, _, _)| *of == run.id)
                .map(|(_, name, status)| LaneDot {
                    name: name.clone(),
                    status: status.as_str().to_owned(),
                })
                .collect();
            summary.stages = stages
                .iter()
                .filter(|(of, _, _)| *of == run.id)
                .map(|(_, stage, state)| StageDot {
                    name: stage.as_str().to_owned(),
                    state: state.as_str().to_owned(),
                })
                .collect();
            summary
        })
        .collect())
}

/// `GET /runs/count`: how many runs the filters of `GET /runs` match, over
/// every page. A cursor narrows it like a page.
pub async fn count(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    ApiQuery(query): ApiQuery<RunQuery>,
) -> ApiResult<RunCount> {
    let filter = query.filter()?;
    let count = dashboard.app.store.count_runs(&filter).await?;
    Ok(Json(RunCount { count }))
}

/// The run a path names, or a 400 or 404.
pub(crate) async fn run_of(app: &App, id: String) -> Result<RunRecord, ApiError> {
    let id = RunId::parse(id).map_err(|_| ApiError::bad_request("That is not a run id."))?;
    app.store
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
    let run = run_of(&dashboard.app, id).await?;
    Ok(Json(run_detail(&dashboard.app, &run).await?))
}

/// Everything the run record holds about `run`: `GET /runs/{id}`, the
/// snapshot a run's stream starts with, and the MCP server's `get_run`.
pub(crate) async fn run_detail(app: &App, run: &RunRecord) -> Result<RunDetail, ApiError> {
    let store = &app.store;
    let calls = store.tool_calls(&run.id).await?;
    let lanes = store
        .lanes(&run.id)
        .await?
        .iter()
        .map(|lane| {
            let mut lane = Lane::from(lane);
            lane.last_call_turn = calls
                .iter()
                .filter(|c| c.session == lane.name)
                .map(|c| c.turn)
                .max();
            lane
        })
        .collect();
    Ok(RunDetail {
        summary: run.summary.clone(),
        error: run.error.clone(),
        check_id: run.check_id.clone(),
        heartbeat_at: run.heartbeat_at.clone(),
        lanes,
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
        stages: store
            .stages(&run.id)
            .await?
            .iter()
            .map(Stage::from)
            .collect(),
        run: RunSummary::from_record(&app.settings, run),
    })
}

/// What `GET /runs/{id}/events` takes.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct EventsQuery {
    pub(crate) limit: Option<u32>,
    pub(crate) cursor: Option<String>,
}

/// `GET /runs/{id}/events`: the run's timeline in order, a page at a time.
/// The cursor is the number of lines already read.
pub async fn events(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<EventsQuery>,
) -> ApiResult<Page<RunEvent>> {
    Ok(Json(run_events(&dashboard.app, id, &query).await?))
}

/// The timeline of run `id`, a page at a time.
pub(crate) async fn run_events(
    app: &App,
    id: String,
    query: &EventsQuery,
) -> Result<Page<RunEvent>, ApiError> {
    let run = run_of(app, id).await?;
    let from = match query.cursor.as_deref() {
        None => 0,
        Some(c) => c
            .parse::<usize>()
            .map_err(|_| ApiError::bad_request("The cursor is not one this API gave out."))?,
    };
    let limit = usize::try_from(limit(query.limit)).unwrap_or(usize::MAX);
    let events = app.store.events(&run.id).await?;
    let items: Vec<RunEvent> = events
        .iter()
        .skip(from)
        .take(limit)
        .map(RunEvent::from)
        .collect();
    let read = from.saturating_add(items.len());
    let next = (read < events.len()).then(|| read.to_string());
    Ok(Page { items, next })
}

/// What `GET /runs/{id}/tool-calls` takes.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolCallQuery {
    pub(crate) session: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) outcome: Option<String>,
}

/// `GET /runs/{id}/tool-calls`: the run's tool calls in order, by session,
/// tool and outcome.
pub async fn tool_calls(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ToolCallQuery>,
) -> ApiResult<Vec<ToolCall>> {
    Ok(Json(run_tool_calls(&dashboard.app, id, &query).await?))
}

/// The tool calls of run `id` in order, by session, tool and outcome.
pub(crate) async fn run_tool_calls(
    app: &App,
    id: String,
    query: &ToolCallQuery,
) -> Result<Vec<ToolCall>, ApiError> {
    let run = run_of(app, id).await?;
    let matches = |wanted: Option<&String>, value: &str| wanted.is_none_or(|w| w == value);
    Ok(app
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
        .collect())
}

/// `GET /runs/{id}/transcripts/{session}`: one session's conversation.
pub async fn transcript(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path((id, session)): Path<(String, String)>,
) -> ApiResult<Transcript> {
    Ok(Json(run_transcript(&dashboard.app, id, &session).await?))
}

/// The conversation of `session` in run `id`.
pub(crate) async fn run_transcript(
    app: &App,
    id: String,
    session: &str,
) -> Result<Transcript, ApiError> {
    let run = run_of(app, id).await?;
    let record = app
        .store
        .transcript(&run.id, session)
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
    Ok(Transcript::from(view))
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
    let who = requester(session.github_id);
    let source = EventSource::Dashboard {
        requester: who.clone(),
    };
    let started = publish_start(&dashboard.bus, &body, source, &who)?;
    Ok((StatusCode::ACCEPTED, Json(started)).into_response())
}

/// Turns a start request into an event and publishes it, as `POST /runs`
/// and the MCP server's start tools do: the listeners apply the allowlist
/// and every refusal. `who` is recorded as the requester.
pub(crate) fn publish_start(
    bus: &Arc<henk_events::EventBus>,
    request: &StartRequest,
    source: EventSource,
    who: &str,
) -> Result<Started, ApiError> {
    let kind = Start::parse(&request.kind)
        .ok_or_else(|| ApiError::bad_request("kind is one of review, plan, address."))?;
    let given =
        |value: Option<&String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let commit = given(request.commit.as_ref());
    let event = start_event(
        kind,
        request.url.trim(),
        commit.as_deref(),
        given(request.note.as_ref()),
        source,
        Some(who.to_owned()),
    )
    .map_err(|message| ApiError::bad_request(format!("{message}.")))?;
    let event_id = publish(bus, event);
    info!(event = %event_id, requester = %who, "started through the API");
    Ok(Started { event_id })
}

/// `POST /runs/{id}/cancel`: cancels a running run, as the dashboard's
/// cancel button does. The request is recorded as an event either way.
pub async fn cancel(
    State(dashboard): State<Arc<Dashboard>>,
    Path(id): Path<String>,
    act: ApiAct<serde_json::Value>,
) -> Result<Response, ApiError> {
    let who = requester(act.session.github_id);
    let cancelled = cancel_run(
        &dashboard.app,
        &dashboard.coordinator,
        id,
        &who,
        "dashboard",
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(cancelled)).into_response())
}

/// Cancels run `id` for `who`, as `POST /runs/{id}/cancel` and the MCP
/// server's `cancel_run` do. The request is recorded as an event from
/// `source` either way.
pub(crate) async fn cancel_run(
    app: &App,
    coordinator: &Coordinator,
    id: String,
    who: &str,
    source: &str,
) -> Result<Cancelled, ApiError> {
    let run = RunId::parse(id).map_err(|_| ApiError::bad_request("That is not a run id."))?;
    let cancelled = coordinator.cancel(&run, who.to_owned());
    record_cancel(app, &run, who, source, cancelled).await;
    if cancelled {
        info!(run = %run, requester = %who, source, "cancelled through the API");
        Ok(Cancelled {
            run_id: run.as_str().to_owned(),
        })
    } else {
        Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict",
            "That run is not running here: it ended, or another Henk process runs it.",
        ))
    }
}

const KINDS: &[(&str, RunKind)] = &[
    ("review", RunKind::Review),
    ("plan", RunKind::Plan),
    ("discord_turn", RunKind::DiscordTurn),
    ("mail_reply", RunKind::MailReply),
    ("address", RunKind::Address),
];

const STATUSES: &[(&str, RunStatus)] = &[
    ("running", RunStatus::Running),
    ("finished", RunStatus::Finished),
    ("failed", RunStatus::Failed),
    ("cancelled", RunStatus::Cancelled),
    ("superseded", RunStatus::Superseded),
];

const PLATFORMS: &[(&str, Platform)] =
    &[("github", Platform::GitHub), ("gitlab", Platform::GitLab)];

/// Who acts, as a stable id: never the login, which is a display name (§2).
fn requester(github_id: u64) -> String {
    format!("github:{github_id}")
}

/// Records a cancel request from `source` and what came of it. A failure
/// is logged; the cancel itself stands.
async fn record_cancel(app: &App, run: &RunId, who: &str, source: &str, cancelled: bool) {
    let store = &app.store;
    let event = InboundEvent {
        id: new_event_id(),
        received_at: now_rfc3339(),
        source: source.to_owned(),
        kind: "cancel_requested".to_owned(),
        repo: None,
        target: None,
        payload: None,
        requester: Some(who.to_owned()),
    };
    let outcome = OutcomeRecord {
        event_id: event.id.clone(),
        listener: source.to_owned(),
        outcome: if cancelled { "cancelled" } else { "ignored" }.to_owned(),
        detail: if cancelled {
            format!("cancel sent to {run}")
        } else {
            format!("{run} is not running here")
        },
        run_id: Some(run.as_str().to_owned()),
        at: String::new(),
    };
    let recorded = match store.record_event(&event).await {
        Ok(()) => store.record_outcome(&outcome).await,
        Err(error) => Err(error),
    };
    if let Err(error) = recorded {
        warn!(%error, run = %run, "could not record a cancel request");
    }
}
