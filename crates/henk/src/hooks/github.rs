//! GitHub webhooks: `POST /webhooks/github`, signed with the webhook secret.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use henk_events::{EventBus, EventSource, Hook, parse_github};
use henk_platform::webhook::verify_github_signature;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{HttpHook, new_event, publish};

/// The GitHub webhook hook.
pub struct GitHubHook {
    secret: Option<SecretString>,
    bus: Arc<EventBus>,
}

impl GitHubHook {
    /// Builds the hook. Without a secret the route answers 503.
    #[must_use]
    pub fn new(secret: Option<SecretString>, bus: Arc<EventBus>) -> Self {
        Self { secret, bus }
    }
}

impl Hook for GitHubHook {
    fn name(&self) -> &'static str {
        "github-webhook"
    }
}

impl HttpHook for GitHubHook {
    fn routes(self: Arc<Self>) -> Router {
        Router::new()
            .route("/webhooks/github", post(receive))
            .with_state(self)
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn receive(State(hook): State<Arc<GitHubHook>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(secret) = &hook.secret else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitHub webhook secret is not configured",
        )
            .into_response();
    };
    if !verify_github_signature(
        secret.expose_secret().as_bytes(),
        header(&headers, "x-hub-signature-256"),
        &body,
    ) {
        warn!("GitHub webhook with a bad signature");
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let event_name = header(&headers, "x-github-event").unwrap_or("").to_owned();
    let delivery = header(&headers, "x-github-delivery")
        .unwrap_or("")
        .to_owned();
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("not JSON: {error}")).into_response();
        }
    };
    let kind = parse_github(&event_name, &payload);
    let event = new_event(EventSource::GitHubWebhook { delivery }, kind, Some(payload));
    info!(event = %event.id, github_event = %event_name, kind = event.kind.name(), "GitHub webhook");
    let id = publish(&hook.bus, event);
    (StatusCode::ACCEPTED, axum::Json(json!({"event": id}))).into_response()
}
