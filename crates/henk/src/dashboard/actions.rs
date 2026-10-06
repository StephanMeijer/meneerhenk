//! What someone signed in may do from the dashboard (#69): start a review,
//! plan or address run, and cancel one that is running. Every allowed id
//! may act (§2, decided in #69); each action is an [`Act`], so it carries
//! the session's CSRF token and comes from the dashboard's own origin.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use henk_domain::run::RunId;
use henk_events::EventSource;
use henk_store::{InboundEvent, OutcomeRecord};
use serde::Deserialize;
use tracing::{info, warn};

use super::Dashboard;
use super::auth::{Act, NoFields, notice, redirect};
use crate::hooks::requests::{Start, start_event};
use crate::hooks::{now_rfc3339, publish};
use crate::ids::new_event_id;

/// The start form.
#[derive(Debug, Deserialize)]
pub struct StartForm {
    kind: String,
    url: String,
    commit: Option<String>,
    note: Option<String>,
}

/// Who acts, as a stable id: never the login, which is a display name (§2).
fn requester(github_id: u64) -> String {
    format!("github:{github_id}")
}

/// An empty form field is no value.
fn given(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Starts a review, plan or address run exactly as `POST /review`, `/plan`
/// and `/address` do: the request becomes an event on the same bus, and the
/// listeners apply the allowlist and every refusal. The browser goes to the
/// event's page, where what the listeners did shows up.
pub async fn start(State(dashboard): State<Arc<Dashboard>>, act: Act<StartForm>) -> Response {
    let Act { session, form } = act;
    let Some(kind) = Start::parse(&form.kind) else {
        return notice(
            StatusCode::BAD_REQUEST,
            "Not started",
            "Choose a review, a plan or an address run.",
        );
    };
    let who = requester(session.github_id);
    let commit = given(form.commit);
    let event = start_event(
        kind,
        form.url.trim(),
        commit.as_deref(),
        given(form.note),
        EventSource::Dashboard {
            requester: who.clone(),
        },
        Some(who.clone()),
    );
    match event {
        Ok(event) => {
            let id = publish(&dashboard.bus, event);
            info!(event = %id, requester = %who, "started from the dashboard");
            redirect(&format!("/dashboard/events/{id}"), None)
        }
        Err(message) => notice(
            StatusCode::BAD_REQUEST,
            "Not started",
            &format!("{message}. Go back and correct it."),
        ),
    }
}

/// Cancels a running review, plan or address run through its cancellation
/// token. The run then ends `cancelled` and says by whom. The request is
/// recorded as an event with its outcome, so the run page shows it.
pub async fn cancel(
    State(dashboard): State<Arc<Dashboard>>,
    Path(id): Path<String>,
    act: Act<NoFields>,
) -> Response {
    let Ok(run) = RunId::parse(id) else {
        return notice(StatusCode::BAD_REQUEST, "Not cancelled", "Bad run id.");
    };
    let who = requester(act.session.github_id);
    let cancelled = dashboard.coordinator.cancel(&run, who.clone());
    record_cancel(&dashboard, &run, &who, cancelled).await;
    if cancelled {
        info!(run = %run, requester = %who, "cancelled from the dashboard");
        redirect(&format!("/dashboard/runs/{run}"), None)
    } else {
        notice(
            StatusCode::CONFLICT,
            "Not cancelled",
            "That run is not running here: it ended, or another Henk process runs it.",
        )
    }
}

/// Records a cancel request and what came of it. A failure is logged; the
/// cancel itself stands.
async fn record_cancel(dashboard: &Dashboard, run: &RunId, who: &str, cancelled: bool) {
    let store = &dashboard.app.store;
    let event = InboundEvent {
        id: new_event_id(),
        received_at: now_rfc3339(),
        source: "dashboard".to_owned(),
        kind: "cancel_requested".to_owned(),
        repo: None,
        target: None,
        payload: None,
        requester: Some(who.to_owned()),
    };
    let outcome = OutcomeRecord {
        event_id: event.id.clone(),
        listener: "dashboard".to_owned(),
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
