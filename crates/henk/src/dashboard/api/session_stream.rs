//! One running session's conversation as it happens (#238), as
//! Server-Sent Events. Messages come from this process's session logs
//! ([`crate::live_session`]); a reconnect with `Last-Event-ID` replays
//! what it missed while the log still holds it, and gets a snapshot when
//! it does not. The stream ends when the session does: its transcript is
//! stored by then and takes over.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::response::sse::Event;
use henk_store::{LaneStatus, RunRecord, RunStatus};
use tokio::sync::{broadcast, mpsc};

use super::ApiError;
use super::runs::run_of;
use super::stream::{BUFFER, StreamQuery, send, sse};
use super::types::{LiveMessage, LiveSnapshot, Message, SessionEnd, SessionMessage};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;
use crate::live_session::{News, Said};
use crate::runs::MessageView;

/// `GET /runs/{id}/sessions/{session}/stream`.
pub async fn session_stream(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: ApiViewer,
    Path((id, session)): Path<(String, String)>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let run = run_of(&dashboard.app, id).await?;
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| query.last().map(str::to_owned));
    // Subscribed before anything is read, so no message falls in between.
    let news = dashboard.app.feed.sessions().subscribe();
    let (out, events) = mpsc::channel(BUFFER);
    tokio::spawn(follow_session(dashboard, run, session, last, news, out));
    Ok(sse(events))
}

/// The seq of `last` when it is an id this process gave out.
fn last_seq(dashboard: &Dashboard, last: Option<&str>) -> Option<u64> {
    let (epoch, seq) = last?.rsplit_once('-')?;
    (epoch == dashboard.app.feed.epoch())
        .then(|| seq.parse().ok())
        .flatten()
}

fn id_of(dashboard: &Dashboard, seq: u64) -> String {
    format!("{}-{seq}", dashboard.app.feed.epoch())
}

/// A kept message as the API shows it; none when it does not read.
fn live(said: &Said) -> Option<LiveMessage> {
    let view = MessageView::parse(&said.body, u64::from(said.turn)).ok()?;
    Some(LiveMessage {
        seq: said.seq,
        at: said.at.clone(),
        message: Message::from(view),
    })
}

/// Whether `session` of `run` runs in this process: it said something
/// here, or its run is ours and its lane has started and not ended.
async fn runs_here(dashboard: &Dashboard, run: &RunRecord, session: &str) -> bool {
    if dashboard.app.feed.sessions().is_live(&run.id, session) {
        return true;
    }
    if !dashboard.app.live_runs.contains(&run.id) {
        return false;
    }
    dashboard.app.store.lanes(&run.id).await.is_ok_and(|lanes| {
        lanes
            .iter()
            .any(|l| l.name == session && l.status == LaneStatus::Running)
    })
}

/// Sends what the follower missed since `last`, or a snapshot. The seq it
/// is now up to; `None` once it has gone.
async fn catch_up(
    dashboard: &Dashboard,
    run: &RunRecord,
    session: &str,
    last: Option<&str>,
    out: &mpsc::Sender<Event>,
) -> Option<u64> {
    let sessions = dashboard.app.feed.sessions();
    if let Some(seq) = last_seq(dashboard, last)
        && let Some(missed) = sessions.since(&run.id, session, seq)
    {
        let mut sent = seq;
        for said in missed {
            if let Some(message) = live(&said)
                && !send(
                    out,
                    &SessionMessage::Message(message),
                    Some(id_of(dashboard, said.seq)),
                )
                .await
            {
                return None;
            }
            sent = said.seq;
        }
        return Some(sent);
    }
    let (kept, cut) = sessions.snapshot(&run.id, session).unwrap_or_default();
    let sent = kept.last().map_or(0, |s| s.seq);
    let snapshot = SessionMessage::Snapshot(LiveSnapshot {
        messages: kept.iter().filter_map(|s| live(s)).collect(),
        cut,
    });
    send(out, &snapshot, Some(id_of(dashboard, sent)))
        .await
        .then_some(sent)
}

/// Feeds one session's stream until it ends or the follower goes.
async fn follow_session(
    dashboard: Arc<Dashboard>,
    run: RunRecord,
    session: String,
    last: Option<String>,
    mut news: broadcast::Receiver<Arc<News>>,
    out: mpsc::Sender<Event>,
) {
    if run.status != RunStatus::Running || !runs_here(&dashboard, &run, &session).await {
        let elsewhere =
            run.status == RunStatus::Running && !dashboard.app.live_runs.contains(&run.id);
        let _ = send(&out, &SessionMessage::End(SessionEnd { elsewhere }), None).await;
        return;
    }
    let Some(mut sent) = catch_up(&dashboard, &run, &session, last.as_deref(), &out).await else {
        return;
    };
    // It may have ended between the first look and the catch-up; its end
    // is then already past, so look again. One that ends after this is
    // heard: the subscription came first.
    if !runs_here(&dashboard, &run, &session).await {
        let _ = send(
            &out,
            &SessionMessage::End(SessionEnd { elsewhere: false }),
            None,
        )
        .await;
        return;
    }
    loop {
        let item = match news.recv().await {
            Ok(item) => item,
            // Too far behind, or Henk is stopping: the follower reconnects
            // and catches up.
            Err(broadcast::error::RecvError::Lagged(_) | broadcast::error::RecvError::Closed) => {
                return;
            }
        };
        if !item.is_of(&run.id, &session) {
            continue;
        }
        match item.as_ref() {
            News::Said { said, .. } if said.seq > sent => {
                sent = said.seq;
                if let Some(message) = live(said)
                    && !send(
                        &out,
                        &SessionMessage::Message(message),
                        Some(id_of(&dashboard, sent)),
                    )
                    .await
                {
                    return;
                }
            }
            News::Said { .. } => {}
            News::Ended { .. } => {
                let end = SessionMessage::End(SessionEnd { elsewhere: false });
                let _ = send(&out, &end, None).await;
                return;
            }
        }
    }
}
