//! The HTTP API: `POST /review`, `/plan`, `/execute`, `/ticket`, `/rebase` and
//! `/fix-ci`
//! behind a bearer token.
//! Each request becomes an event like any webhook; the listeners do the rest.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use henk_events::{EventBus, EventSource, Hook};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq as _;

use super::requests::{Start, start_event};
use super::{HttpHook, publish};

/// The API hook.
pub struct ApiHook {
    token: Option<SecretString>,
    requester: Option<String>,
    bus: Arc<EventBus>,
    /// The hosts a URL may name (#58).
    hosts: crate::urls::Hosts,
}

impl ApiHook {
    /// Builds the hook. `requester` is the Team Lead id the events' source
    /// records; the runs they start name no requester (#399). A URL must be
    /// on one of `hosts`.
    #[must_use]
    pub fn new(
        token: Option<SecretString>,
        requester: Option<String>,
        bus: Arc<EventBus>,
        hosts: crate::urls::Hosts,
    ) -> Self {
        Self {
            token,
            requester,
            bus,
            hosts,
        }
    }

    fn unauthorized(&self, headers: &HeaderMap) -> Option<Response> {
        let Some(expected) = &self.token else {
            return Some(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "API token is not configured",
                )
                    .into_response(),
            );
        };
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if bool::from(given.as_bytes().ct_eq(expected.expose_secret().as_bytes())) {
            None
        } else {
            Some((StatusCode::UNAUTHORIZED, "bad token").into_response())
        }
    }
}

impl Hook for ApiHook {
    fn name(&self) -> &'static str {
        "api"
    }
}

impl HttpHook for ApiHook {
    fn routes(self: Arc<Self>) -> Router {
        Router::new()
            .route("/review", post(review))
            .route("/plan", post(plan))
            .route("/execute", post(execute))
            .route("/ticket", post(ticket))
            .route("/rebase", post(rebase))
            .route("/fix-ci", post(fix_ci))
            .with_state(self)
    }
}

#[derive(Debug, Deserialize)]
struct ReviewBody {
    url: String,
    commit: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UrlBody {
    url: String,
}

#[derive(Debug, Deserialize)]
struct NoteBody {
    url: String,
    note: Option<String>,
}

async fn review(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    match serde_json::from_slice::<ReviewBody>(&body) {
        Ok(request) => start(
            &hook,
            &headers,
            Start::Review,
            &request.url,
            request.commit.as_deref(),
            None,
        ),
        Err(error) => bad_body(&hook, &headers, &error),
    }
}

async fn plan(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    match serde_json::from_slice::<NoteBody>(&body) {
        Ok(request) => start(
            &hook,
            &headers,
            Start::Plan,
            &request.url,
            None,
            request.note,
        ),
        Err(error) => bad_body(&hook, &headers, &error),
    }
}

async fn execute(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    match serde_json::from_slice::<NoteBody>(&body) {
        Ok(request) => start(
            &hook,
            &headers,
            Start::Execute,
            &request.url,
            None,
            request.note,
        ),
        Err(error) => bad_body(&hook, &headers, &error),
    }
}

async fn ticket(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    match serde_json::from_slice::<NoteBody>(&body) {
        Ok(request) => start(
            &hook,
            &headers,
            Start::Ticket,
            &request.url,
            None,
            request.note,
        ),
        Err(error) => bad_body(&hook, &headers, &error),
    }
}

async fn rebase(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    match serde_json::from_slice::<UrlBody>(&body) {
        Ok(request) => start(&hook, &headers, Start::Rebase, &request.url, None, None),
        Err(error) => bad_body(&hook, &headers, &error),
    }
}

async fn fix_ci(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    match serde_json::from_slice::<UrlBody>(&body) {
        Ok(request) => start(&hook, &headers, Start::FixCi, &request.url, None, None),
        Err(error) => bad_body(&hook, &headers, &error),
    }
}

/// A body that does not parse; still `401` first when the token is wrong.
fn bad_body(hook: &ApiHook, headers: &HeaderMap, error: &serde_json::Error) -> Response {
    hook.unauthorized(headers)
        .unwrap_or_else(|| (StatusCode::BAD_REQUEST, format!("bad body: {error}")).into_response())
}

/// Checks the token, builds the event and publishes it: `202` with its id.
/// Refusals (the allowlist, a closed pull request) come later, as the
/// listeners' outcomes on the event.
fn start(
    hook: &ApiHook,
    headers: &HeaderMap,
    start: Start,
    url: &str,
    commit: Option<&str>,
    note: Option<String>,
) -> Response {
    if let Some(response) = hook.unauthorized(headers) {
        return response;
    }
    let source = EventSource::Api {
        requester: hook.requester.clone(),
    };
    // The token names nobody, so no run it starts names a requester, and
    // no commit credits one (#399). A plan still runs for
    // `[planning] requester_id`, which `plan.rs` falls back to.
    let requester = None;
    match start_event(start, url, commit, note, source, requester, &hook.hosts) {
        Ok(event) => {
            let id = publish(&hook.bus, event);
            (StatusCode::ACCEPTED, axum::Json(json!({"event": id}))).into_response()
        }
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}
