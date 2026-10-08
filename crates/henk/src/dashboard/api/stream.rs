//! The live streams (#202), as Server-Sent Events: one run as it happens,
//! and what runs now. Changes come from this process's feed
//! ([`crate::live`]); a reconnect with `Last-Event-ID` replays what it
//! missed, or starts again from a snapshot when the feed no longer holds
//! it. A run another Henk process works on is not on this feed, so its
//! stream sends a fresh snapshot now and then until it ends.
//!
//! Each stream is fed by a task of its own through a small channel. A
//! follower that falls too far behind the feed is dropped and reconnects;
//! nothing it does can hold up a run.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use henk_domain::run::RunId;
use henk_store::{Page, RunFilter, RunRecord, RunStatus};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};

use super::ApiError;
use super::runs::{run_detail, run_of};
use super::types::{
    Draft, Finding, Heartbeat, Lane, LaneDot, Progress, RunDetail, RunEvent, RunMessage,
    RunSummary, RunUpdate, RunningMessage, RunningSnapshot, Slots, Stage, StageDot, ToolCall,
    TranscriptRef, WaitingReview,
};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;
use crate::live::{Change, ChangeKind};

/// Messages waiting for a slow follower before its task waits too.
pub(super) const BUFFER: usize = 64;

/// How often a keep-alive comment goes out.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// What a stream's URL may carry besides the `Last-Event-ID` header.
#[derive(Debug, Default, Deserialize)]
pub struct StreamQuery {
    /// The last id seen, for a client that cannot set the header.
    last: Option<String>,
}

impl StreamQuery {
    /// The last id seen, from the URL.
    pub(super) fn last(&self) -> Option<&str> {
        self.last.as_deref()
    }
}

/// `GET /runs/{id}/stream`.
pub async fn run_stream(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path(id): Path<String>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let run = run_of(&dashboard, id).await?;
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or(query.last);
    // Subscribed before anything is read, so no change falls in between.
    let changes = dashboard.app.feed.subscribe();
    let (out, events) = mpsc::channel(BUFFER);
    tokio::spawn(follow_run(dashboard, run, last, changes, out));
    Ok(sse(events))
}

/// `GET /runs/stream`.
pub async fn running_stream(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
) -> Response {
    let changes = dashboard.app.feed.subscribe();
    let (out, events) = mpsc::channel(BUFFER);
    tokio::spawn(follow_running(dashboard, changes, out));
    sse(events)
}

pub(super) fn sse(events: mpsc::Receiver<Event>) -> Response {
    let stream = stream::unfold(events, |mut events| async move {
        events
            .recv()
            .await
            .map(|event| (Ok::<_, Infallible>(event), events))
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response();
    // A proxy in between must pass each message on, not hold them.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// The SSE event of a message: its `kind` as the event, its `data` as JSON.
fn event<T: serde::Serialize>(message: &T, id: Option<String>) -> Option<Event> {
    let value = serde_json::to_value(message).ok()?;
    let kind = value.get("kind")?.as_str()?.to_owned();
    let data = value
        .get("data")
        .map_or_else(|| "{}".to_owned(), ToString::to_string);
    let event = Event::default().event(kind).data(data);
    Some(match id {
        Some(id) => event.id(id),
        None => event,
    })
}

/// A change as the message the stream sends.
fn message(dashboard: &Dashboard, change: &Change) -> RunMessage {
    match &change.kind {
        ChangeKind::Run(run) => RunMessage::Run(update(dashboard, run)),
        ChangeKind::Lanes(lanes) => RunMessage::Lanes(lanes.iter().map(Lane::from).collect()),
        ChangeKind::ToolCall(call) => RunMessage::ToolCall(ToolCall::from(call)),
        ChangeKind::Draft(draft) => RunMessage::Draft(Draft::from(draft)),
        ChangeKind::Finding(finding) => RunMessage::Finding(Finding::from(finding)),
        ChangeKind::Event(line) => RunMessage::Event(RunEvent::from(line)),
        ChangeKind::Transcript(t) => RunMessage::Transcript(TranscriptRef::from(t)),
        ChangeKind::Stages(stages) => RunMessage::Stages(stages.iter().map(Stage::from).collect()),
        ChangeKind::Heartbeat(at) => RunMessage::Heartbeat(Heartbeat { at: at.clone() }),
    }
}

fn update(dashboard: &Dashboard, run: &RunRecord) -> RunUpdate {
    RunUpdate {
        run: RunSummary::from_record(&dashboard.app.settings, run),
        summary: run.summary.clone(),
        error: run.error.clone(),
        check_id: run.check_id.clone(),
    }
}

/// `<epoch>-<seq>`, the id of a message from this process's feed.
fn id_of(dashboard: &Dashboard, seq: u64) -> String {
    format!("{}-{seq}", dashboard.app.feed.epoch())
}

/// The changes a follower missed since `last`, or `None` when it needs a
/// snapshot.
fn missed(dashboard: &Dashboard, last: Option<&str>, run: &RunRecord) -> Option<Vec<Arc<Change>>> {
    let (epoch, seq) = last?.rsplit_once('-')?;
    let seq = seq.parse().ok()?;
    dashboard.app.feed.since(epoch, seq, &run.id)
}

/// Sends `message`; false once the follower has gone.
pub(super) async fn send<T: serde::Serialize>(
    out: &mpsc::Sender<Event>,
    message: &T,
    id: Option<String>,
) -> bool {
    match event(message, id) {
        Some(event) => out.send(event).await.is_ok(),
        None => true,
    }
}

/// The run as the store holds it, the seq it is current to, and whether
/// it has ended. Read while the feed is settled, so every change up to the
/// seq is in it and none after: nothing the stream sends next repeats it.
async fn snapshot(dashboard: &Dashboard, id: &RunId) -> Option<(u64, RunDetail, bool)> {
    let _settled = dashboard.app.feed.settled().await;
    let seq = dashboard.app.feed.last();
    let run = dashboard.app.store.run(id).await.ok()??;
    let detail = run_detail(dashboard, &run).await.ok()?;
    Some((seq, detail, run.status != RunStatus::Running))
}

/// Sends a snapshot. The seq it is current to and whether the run has
/// ended; `None` once the follower has gone or the run cannot be read.
async fn send_snapshot(
    dashboard: &Dashboard,
    id: &RunId,
    out: &mpsc::Sender<Event>,
) -> Option<(u64, bool)> {
    let (seq, detail, ended) = snapshot(dashboard, id).await?;
    send(
        out,
        &RunMessage::Snapshot(Box::new(detail)),
        Some(id_of(dashboard, seq)),
    )
    .await
    .then_some((seq, ended))
}

/// Sends a follower what it missed since `last`, or a snapshot when the
/// feed cannot say. The seq it is now up to and whether what it was sent
/// shows the run ended; `None` once it has gone.
async fn catch_up(
    dashboard: &Dashboard,
    run: &RunRecord,
    last: Option<&str>,
    out: &mpsc::Sender<Event>,
) -> Option<(u64, bool)> {
    let Some(replay) = missed(dashboard, last, run) else {
        return send_snapshot(dashboard, &run.id, out).await;
    };
    let mut sent = last
        .and_then(|l| l.rsplit_once('-'))
        .and_then(|(_, s)| s.parse().ok())
        .unwrap_or(0);
    let mut ended = false;
    for change in replay {
        let id = Some(id_of(dashboard, change.seq));
        if !send(out, &message(dashboard, &change), id).await {
            return None;
        }
        sent = change.seq;
        if let ChangeKind::Run(r) = &change.kind {
            ended = r.status != RunStatus::Running;
        }
    }
    Some((sent, ended))
}

/// Feeds one run's stream until the run ends or the follower goes.
async fn follow_run(
    dashboard: Arc<Dashboard>,
    run: RunRecord,
    last: Option<String>,
    mut changes: broadcast::Receiver<Arc<Change>>,
    out: mpsc::Sender<Event>,
) {
    let Some((mut sent, shown_ended)) = catch_up(&dashboard, &run, last.as_deref(), &out).await
    else {
        return;
    };
    let ended = shown_ended
        || match dashboard.app.store.run(&run.id).await {
            Ok(Some(now)) if now.status == RunStatus::Running => false,
            // It ended after what the follower was sent, and that change
            // may still be on its way: the run as it ended goes first, so
            // the page does not stay running.
            Ok(Some(_)) => {
                if send_snapshot(&dashboard, &run.id, &out).await.is_none() {
                    return;
                }
                true
            }
            Ok(None) | Err(_) => true,
        };
    if ended {
        let _ = send(&out, &RunMessage::End, None).await;
        return;
    }
    if !dashboard.app.live_runs.contains(&run.id) {
        refresh_elsewhere(&dashboard, &run, &out).await;
        return;
    }
    loop {
        let change = match changes.recv().await {
            Ok(change) => change,
            // Too far behind, or Henk is stopping: the follower reconnects
            // and catches up.
            Err(broadcast::error::RecvError::Lagged(_) | broadcast::error::RecvError::Closed) => {
                return;
            }
        };
        if change.run != run.id || change.seq <= sent {
            continue;
        }
        sent = change.seq;
        let ended = matches!(&change.kind, ChangeKind::Run(r) if r.status != RunStatus::Running);
        if !send(
            &out,
            &message(&dashboard, &change),
            Some(id_of(&dashboard, change.seq)),
        )
        .await
        {
            return;
        }
        if ended {
            let _ = send(&out, &RunMessage::End, None).await;
            return;
        }
    }
}

/// A run another process works on: a snapshot every
/// [`Dashboard::refresh`] until it ends.
async fn refresh_elsewhere(dashboard: &Dashboard, run: &RunRecord, out: &mpsc::Sender<Event>) {
    loop {
        tokio::time::sleep(dashboard.refresh).await;
        let Ok(Some(now)) = dashboard.app.store.run(&run.id).await else {
            return;
        };
        let Ok(detail) = run_detail(dashboard, &now).await else {
            return;
        };
        if !send(out, &RunMessage::Snapshot(Box::new(detail)), None).await {
            return;
        }
        if now.status != RunStatus::Running {
            let _ = send(out, &RunMessage::End, None).await;
            return;
        }
    }
}

/// What runs now, as the overview shows it.
async fn running_now(dashboard: &Dashboard) -> Option<RunningSnapshot> {
    let filter = RunFilter {
        status: Some(RunStatus::Running),
        ..RunFilter::default()
    };
    let store = &dashboard.app.store;
    let runs = store
        .list_runs(&filter, Page::new(Page::MAX, 0))
        .await
        .ok()?;
    let count = store.count_runs(&filter).await.ok()?;
    let summaries = super::runs::summaries(dashboard, &runs).await.ok()?;
    Some(RunningSnapshot {
        count: count.max(u64::try_from(runs.len()).unwrap_or(u64::MAX)),
        runs: summaries,
        slots: slots(dashboard),
    })
}

/// The coordinator's review slots, as the overview shows them.
fn slots(dashboard: &Dashboard) -> Slots {
    let now = dashboard.coordinator.slots();
    let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    Slots {
        limit: count(now.limit),
        in_use: count(now.in_use),
        waiting: now
            .waiting
            .into_iter()
            .map(|w| WaitingReview {
                repo: w.repo,
                target: w.number,
                since: w
                    .since
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            })
            .collect(),
    }
}

/// A running run's lanes or stages as the overview's dots, from a change
/// on the feed: no store read.
fn progress(change: &Change) -> Option<Progress> {
    let run_id = change.run.as_str().to_owned();
    match &change.kind {
        ChangeKind::Lanes(lanes) => Some(Progress {
            run_id,
            lanes: Some(
                lanes
                    .iter()
                    .map(|lane| LaneDot {
                        name: lane.name.clone(),
                        status: lane.status.as_str().to_owned(),
                    })
                    .collect(),
            ),
            stages: None,
        }),
        ChangeKind::Stages(stages) => Some(Progress {
            run_id,
            lanes: None,
            stages: Some(
                stages
                    .iter()
                    .map(|stage| StageDot {
                        name: stage.stage.as_str().to_owned(),
                        state: stage.state.as_str().to_owned(),
                    })
                    .collect(),
            ),
        }),
        _ => None,
    }
}

/// Feeds the overview's stream: a snapshot now and every
/// [`Dashboard::refresh`] times six (runs of other processes, and any
/// drift), and every run that starts or ends here in between, and the
/// slots each time they change.
async fn follow_running(
    dashboard: Arc<Dashboard>,
    mut changes: broadcast::Receiver<Arc<Change>>,
    out: mpsc::Sender<Event>,
) {
    let mut every = tokio::time::interval(dashboard.refresh.saturating_mul(6));
    let mut slots_changed = dashboard.coordinator.watch_slots();
    loop {
        tokio::select! {
            Ok(()) = slots_changed.changed() => {
                if !send(&out, &RunningMessage::Slots(slots(&dashboard)), None).await {
                    return;
                }
            }
            _ = every.tick() => {
                let Some(snapshot) = running_now(&dashboard).await else { return };
                if !send(&out, &RunningMessage::Snapshot(snapshot), None).await {
                    return;
                }
            }
            change = changes.recv() => {
                let Ok(change) = change else { return };
                if let ChangeKind::Run(run) = &change.kind {
                    let summary = RunSummary::from_record(&dashboard.app.settings, run);
                    if !send(&out, &RunningMessage::Run(Box::new(summary)), None).await {
                        return;
                    }
                } else if let Some(moved) = progress(&change)
                    && !send(&out, &RunningMessage::Progress(moved), None).await
                {
                    return;
                }
            }
        }
    }
}
