//! The HTTP server: webhooks, the API for reviews and plans, run pages.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use henk_domain::run::RunId;
use henk_platform::webhook::{verify_github_signature, verify_gitlab_token};
use henk_platform::{parse_github, parse_gitlab};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::app::{App, env_var};
use crate::coordinator::Coordinator;
use crate::dispatch::{Dispatched, Dispatcher};
use crate::review::ReviewRequest;

/// What every handler can reach.
pub struct Shared {
    coordinator: Arc<Coordinator>,
    dispatcher: Dispatcher,
    github_secret: Option<SecretString>,
    gitlab_token: Option<SecretString>,
    api_token: Option<SecretString>,
}

/// Builds the router. Secrets come from the environment variables the
/// settings name; a missing one disables its route with a clear status.
pub fn router(app: Arc<App>) -> Router {
    let server = &app.settings.server;
    let github_secret = env_var(&server.github_webhook_secret_env).map(SecretString::from);
    let gitlab_token = env_var(&server.gitlab_webhook_token_env).map(SecretString::from);
    let api_token = env_var(&server.api_token_env).map(SecretString::from);
    for (name, present) in [
        (
            server.github_webhook_secret_env.as_str(),
            github_secret.is_some(),
        ),
        (
            server.gitlab_webhook_token_env.as_str(),
            gitlab_token.is_some(),
        ),
        (server.api_token_env.as_str(), api_token.is_some()),
    ] {
        if !present {
            warn!(
                variable = name,
                "not set; the routes that need it answer 503"
            );
        }
    }
    router_with_secrets(app, github_secret, gitlab_token, api_token)
}

/// Builds the router with explicit secrets, for tests and for `router`.
pub fn router_with_secrets(
    app: Arc<App>,
    github_secret: Option<SecretString>,
    gitlab_token: Option<SecretString>,
    api_token: Option<SecretString>,
) -> Router {
    let coordinator = Arc::new(Coordinator::new(app));
    let dispatcher = Dispatcher::new(Arc::clone(&coordinator));
    let shared = Arc::new(Shared {
        coordinator,
        dispatcher,
        github_secret,
        gitlab_token,
        api_token,
    });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/runs/{id}", get(run_page))
        .route("/webhooks/github", post(github_webhook))
        .route("/webhooks/gitlab", post(gitlab_webhook))
        .route("/review", post(api_review))
        .route("/plan", post(api_plan))
        .layer(TraceLayer::new_for_http())
        .with_state(shared)
}

async fn healthz(State(shared): State<Arc<Shared>>) -> Response {
    axum::Json(json!({"status": "ok", "active_reviews": shared.coordinator.active_reviews()}))
        .into_response()
}

/// Serves until SIGINT or SIGTERM.
///
/// # Errors
///
/// Returns an error when the bind address is unusable.
pub async fn serve(app: Arc<App>) -> anyhow::Result<()> {
    let bind = app.settings.server.bind.clone();
    let router = router(app);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    info!(%bind, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving")?;
    info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(error) => {
                    warn!(%error, "no SIGTERM handler; waiting for ctrl-c only");
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

async fn github_webhook(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(secret) = &shared.github_secret else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitHub webhook secret is not configured",
        )
            .into_response();
    };
    if !verify_github_signature(
        secret.expose_secret().as_bytes(),
        header_str(&headers, "x-hub-signature-256"),
        &body,
    ) {
        warn!("GitHub webhook with a bad signature");
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let event = header_str(&headers, "x-github-event")
        .unwrap_or("")
        .to_owned();
    let delivery = header_str(&headers, "x-github-delivery")
        .unwrap_or("")
        .to_owned();
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("not JSON: {error}")).into_response();
        }
    };
    let parsed = parse_github(&event, &payload);
    info!(%event, %delivery, ?parsed, "GitHub webhook");
    dispatch_in_background(shared, parsed);
    StatusCode::ACCEPTED.into_response()
}

async fn gitlab_webhook(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(token) = &shared.gitlab_token else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitLab webhook token is not configured",
        )
            .into_response();
    };
    if !verify_gitlab_token(
        token.expose_secret(),
        header_str(&headers, "x-gitlab-token"),
    ) {
        warn!("GitLab webhook with a bad token");
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let event = header_str(&headers, "x-gitlab-event")
        .unwrap_or("")
        .to_owned();
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("not JSON: {error}")).into_response();
        }
    };
    let parsed = parse_gitlab(&event, &payload);
    info!(%event, ?parsed, "GitLab webhook");
    dispatch_in_background(shared, parsed);
    StatusCode::ACCEPTED.into_response()
}

fn dispatch_in_background(shared: Arc<Shared>, event: henk_platform::IncomingEvent) {
    tokio::spawn(async move {
        match shared.dispatcher.handle(event).await {
            Dispatched::Ignored(reason) => info!(%reason, "event ignored"),
            Dispatched::Review(decision) => info!(?decision, "review dispatched"),
            Dispatched::Greeted => info!("greeted"),
        }
    });
}

fn unauthorized(shared: &Shared, headers: &HeaderMap) -> Option<Response> {
    let Some(expected) = &shared.api_token else {
        return Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "API token is not configured",
            )
                .into_response(),
        );
    };
    let given = header_str(headers, header::AUTHORIZATION.as_str())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if bool::from(given.as_bytes().ct_eq(expected.expose_secret().as_bytes())) {
        None
    } else {
        Some((StatusCode::UNAUTHORIZED, "bad token").into_response())
    }
}

#[derive(Debug, Deserialize)]
struct ReviewBody {
    url: String,
    commit: Option<String>,
}

async fn api_review(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = unauthorized(&shared, &headers) {
        return response;
    }
    let request: ReviewBody = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("bad body: {error}")).into_response();
        }
    };
    let target = match crate::urls::parse_pull_request_url(&request.url) {
        Ok(target) => target,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let app = shared.coordinator.app();
    let writer = match app.writer(target.platform()) {
        Ok(writer) => writer,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let commit = match request.commit {
        Some(sha) => match henk_domain::review::CommitSha::parse(&sha) {
            Ok(sha) => sha,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        },
        None => match writer.pull_request(&target).await {
            Ok(info) => info.head,
            Err(error) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
        },
    };
    let requester = app
        .settings
        .planning
        .as_ref()
        .map(|p| p.requester_id.to_string());
    let decision = shared.coordinator.submit_review(
        ReviewRequest {
            target,
            commit: Some(commit.clone()),
            trigger: "api".to_owned(),
            requester,
            acknowledge: None,
        },
        commit,
    );
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({"decision": format!("{decision:?}")})),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct PlanBody {
    url: String,
    note: Option<String>,
}

async fn api_plan(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(response) = unauthorized(&shared, &headers) {
        return response;
    }
    let request: PlanBody = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("bad body: {error}")).into_response();
        }
    };
    let target = match crate::urls::parse_issue_url(&request.url) {
        Ok(target) => target,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    shared
        .coordinator
        .submit_plan(target, request.note, "api".to_owned());
    (
        StatusCode::ACCEPTED,
        axum::Json(json!({"status": "planning"})),
    )
        .into_response()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn run_page(State(shared): State<Arc<Shared>>, Path(id): Path<String>) -> Response {
    let Ok(run_id) = RunId::parse(id.clone()) else {
        return (StatusCode::BAD_REQUEST, "bad run id").into_response();
    };
    let store = &shared.coordinator.app().store;
    let run = match store.run(&run_id) {
        Ok(Some(run)) => run,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let lanes = store.lanes(&run_id).unwrap_or_default();
    let events = store.events(&run_id).unwrap_or_default();

    let mut html = String::new();
    html.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Run ");
    html.push_str(&escape(run.id.as_str()));
    html.push_str("</title><style>body{font:15px/1.5 system-ui,sans-serif;max-width:60rem;margin:2rem auto;padding:0 1rem;color:#222}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:.3rem .6rem;border-bottom:1px solid #ddd}code{background:#f3f3f3;padding:0 .2rem}</style></head><body>");
    let _ = write!(
        html,
        "<h1>Meneer Henk: {} {}</h1><p><b>{}</b> {} #{} {}<br>Status: <b>{:?}</b><br>Started {}{}<br>Trigger: {}{}</p>",
        escape(&format!("{:?}", run.kind).to_lowercase()),
        escape(run.id.as_str()),
        escape(&format!("{:?}", run.platform)),
        escape(&run.repo),
        run.target,
        run.commit
            .as_deref()
            .map(|c| format!("at <code>{}</code>", escape(c)))
            .unwrap_or_default(),
        run.status,
        escape(&run.started_at),
        run.finished_at
            .as_deref()
            .map(|f| format!(", finished {}", escape(f)))
            .unwrap_or_default(),
        escape(&run.trigger),
        run.requester
            .as_deref()
            .map(|r| format!(" (asked by {})", escape(r)))
            .unwrap_or_default(),
    );
    if let Some(summary) = &run.summary {
        let _ = write!(html, "<p><b>Summary:</b> {}</p>", escape(summary));
    }
    if let Some(error) = &run.error {
        let _ = write!(html, "<p><b>Error:</b> <code>{}</code></p>", escape(error));
    }
    if !lanes.is_empty() {
        html.push_str("<h2>Lanes</h2><table><tr><th>Lane</th><th>Model</th><th>Status</th><th>Turns</th><th>Tokens in</th><th>Tokens out</th><th>Error</th></tr>");
        for lane in &lanes {
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{:?}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&lane.name),
                escape(&lane.model),
                lane.status,
                lane.turns,
                lane.input_tokens,
                lane.output_tokens,
                escape(lane.error.as_deref().unwrap_or(""))
            );
        }
        html.push_str("</table>");
    }
    if !events.is_empty() {
        html.push_str("<h2>Timeline</h2><table>");
        for event in &events {
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&event.at),
                escape(&event.level),
                escape(&event.message)
            );
        }
        html.push_str("</table>");
    }
    html.push_str("</body></html>");
    Html(html).into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use axum::body::Body;
    use axum::http::Request;
    use hmac::{Hmac, KeyInit as _, Mac as _};
    use http_body_util::BodyExt as _;
    use sha2::Sha256;
    use tower::ServiceExt as _;

    use super::*;
    use crate::config::Config;

    const MINIMAL: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
"#;

    async fn test_router() -> Router {
        let settings = Config::parse(MINIMAL).unwrap().into_settings().unwrap();
        let app = Arc::new(
            App::build(settings, Some(std::path::Path::new(":memory:")))
                .await
                .unwrap(),
        );
        router_with_secrets(
            app,
            Some(SecretString::from("whsec".to_owned())),
            Some(SecretString::from("gltok".to_owned())),
            Some(SecretString::from("apitok".to_owned())),
        )
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        bytes.iter().fold("sha256=".to_owned(), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    async fn status_and_body(router: Router, request: Request<Body>) -> (StatusCode, String) {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn healthz_reports_ok() {
        let (status, body) = status_and_body(
            test_router().await,
            Request::get("/healthz").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"active_reviews\":0"));
    }

    #[tokio::test]
    async fn github_webhook_checks_the_signature() {
        let payload = br#"{"zen":"keep it simple","repository":{"full_name":"docspec/app"},"sender":{"login":"a","type":"User"}}"#;
        let bad = Request::post("/webhooks/github")
            .header("x-github-event", "ping")
            .header("x-hub-signature-256", "sha256=00")
            .body(Body::from(payload.to_vec()))
            .unwrap();
        assert_eq!(
            status_and_body(test_router().await, bad).await.0,
            StatusCode::UNAUTHORIZED
        );

        let good = Request::post("/webhooks/github")
            .header("x-github-event", "ping")
            .header("x-hub-signature-256", sign("whsec", payload))
            .body(Body::from(payload.to_vec()))
            .unwrap();
        assert_eq!(
            status_and_body(test_router().await, good).await.0,
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn gitlab_webhook_checks_the_token() {
        let payload = br#"{"object_kind":"push","project":{"path_with_namespace":"9xxlab/app"}}"#;
        let bad = Request::post("/webhooks/gitlab")
            .header("x-gitlab-token", "nope")
            .body(Body::from(payload.to_vec()))
            .unwrap();
        assert_eq!(
            status_and_body(test_router().await, bad).await.0,
            StatusCode::UNAUTHORIZED
        );
        let good = Request::post("/webhooks/gitlab")
            .header("x-gitlab-token", "gltok")
            .body(Body::from(payload.to_vec()))
            .unwrap();
        assert_eq!(
            status_and_body(test_router().await, good).await.0,
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn api_requires_the_bearer_token_and_a_valid_url() {
        let no_token = Request::post("/review")
            .body(Body::from(
                r#"{"url":"https://github.com/docspec/app/pull/1"}"#,
            ))
            .unwrap();
        assert_eq!(
            status_and_body(test_router().await, no_token).await.0,
            StatusCode::UNAUTHORIZED
        );
        let bad_url = Request::post("/plan")
            .header("authorization", "Bearer apitok")
            .body(Body::from(r#"{"url":"https://example.com/x"}"#))
            .unwrap();
        assert_eq!(
            status_and_body(test_router().await, bad_url).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn unknown_runs_are_not_found() {
        let (status, _) = status_and_body(
            test_router().await,
            Request::get("/runs/r-nope").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = status_and_body(
            test_router().await,
            Request::get("/runs/bad%20id").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
