//! The HTTP API: `POST /review` and `POST /plan` behind a bearer token.
//! Each request becomes an event like any webhook; the listeners do the rest.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use henk_domain::review::CommitSha;
use henk_events::{EventBus, EventKind, EventSource, Hook};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq as _;

use super::{HttpHook, new_event, publish};
use crate::urls::{parse_issue_url, parse_pull_request_url};

/// The API hook.
pub struct ApiHook {
    token: Option<SecretString>,
    requester: Option<String>,
    bus: Arc<EventBus>,
}

impl ApiHook {
    /// Builds the hook. `requester` is the Team Lead id API work runs for.
    #[must_use]
    pub fn new(token: Option<SecretString>, requester: Option<String>, bus: Arc<EventBus>) -> Self {
        Self {
            token,
            requester,
            bus,
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
            .with_state(self)
    }
}

#[derive(Debug, Deserialize)]
struct ReviewBody {
    url: String,
    commit: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlanBody {
    url: String,
    note: Option<String>,
}

async fn review(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = hook.unauthorized(&headers) {
        return response;
    }
    let request: ReviewBody = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("bad body: {error}")).into_response();
        }
    };
    let target = match parse_pull_request_url(&request.url) {
        Ok(target) => target,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let commit = match request.commit.as_deref().map(CommitSha::parse).transpose() {
        Ok(commit) => commit,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let event = new_event(
        EventSource::Api {
            requester: hook.requester.clone(),
        },
        EventKind::ReviewRequested {
            target,
            commit,
            requester: hook.requester.clone(),
        },
        None,
    );
    let id = publish(&hook.bus, event);
    (StatusCode::ACCEPTED, axum::Json(json!({"event": id}))).into_response()
}

async fn plan(State(hook): State<Arc<ApiHook>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = hook.unauthorized(&headers) {
        return response;
    }
    let request: PlanBody = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("bad body: {error}")).into_response();
        }
    };
    let target = match parse_issue_url(&request.url) {
        Ok(target) => target,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let event = new_event(
        EventSource::Api {
            requester: hook.requester.clone(),
        },
        EventKind::PlanRequested {
            target,
            note: request.note,
            requester: hook.requester.clone(),
        },
        None,
    );
    let id = publish(&hook.bus, event);
    (StatusCode::ACCEPTED, axum::Json(json!({"event": id}))).into_response()
}
