//! GitLab webhooks: `POST /webhooks/gitlab`, authenticated by the shared token.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use henk_events::{EventBus, EventSource, Hook, parse_gitlab};
use henk_platform::webhook::verify_gitlab_token;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{HttpHook, new_event, publish};

/// The GitLab webhook hook.
pub struct GitLabHook {
    token: Option<SecretString>,
    bus: Arc<EventBus>,
}

impl GitLabHook {
    /// Builds the hook. Without a token the route answers 503.
    #[must_use]
    pub fn new(token: Option<SecretString>, bus: Arc<EventBus>) -> Self {
        Self { token, bus }
    }
}

impl Hook for GitLabHook {
    fn name(&self) -> &'static str {
        "gitlab-webhook"
    }
}

impl HttpHook for GitLabHook {
    fn routes(self: Arc<Self>) -> Router {
        Router::new()
            .route("/webhooks/gitlab", post(receive))
            .with_state(self)
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn receive(State(hook): State<Arc<GitLabHook>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(token) = &hook.token else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitLab webhook token is not configured",
        )
            .into_response();
    };
    if !verify_gitlab_token(token.expose_secret(), header(&headers, "x-gitlab-token")) {
        warn!("GitLab webhook with a bad token");
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let event_name = header(&headers, "x-gitlab-event").unwrap_or("").to_owned();
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("not JSON: {error}")).into_response();
        }
    };
    let kind = parse_gitlab(&event_name, &payload);
    let event = new_event(
        EventSource::GitLabWebhook {
            event: event_name.clone(),
        },
        kind,
        Some(payload),
    );
    info!(event = %event.id, gitlab_event = %event_name, kind = event.kind.name(), "GitLab webhook");
    let id = publish(&hook.bus, event);
    (StatusCode::ACCEPTED, axum::Json(json!({"event": id}))).into_response()
}
