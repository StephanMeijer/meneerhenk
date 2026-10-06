//! The HTTP server: composition of hooks, listeners and the bus, plus the
//! pages that make runs and events traceable (§8.6).

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use henk_domain::run::{EventId, RunId};
use henk_events::{EventBus, Hook};
use secrecy::SecretString;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::app::{App, env_var};
use crate::coordinator::Coordinator;
use crate::hooks::{ApiHook, GitHubHook, GitLabHook, HttpHook};
use crate::listeners::{MentionListener, PlanListener, ReviewListener, Writers};
use crate::recorder::StoreRecorder;

/// What the server's own pages can reach.
pub struct Shared {
    coordinator: Arc<Coordinator>,
    bus: Arc<EventBus>,
}

/// The composed server: the router and the hooks that may need to run.
pub struct Composed {
    /// Every route.
    pub router: Router,
    /// The hooks, for their long-running parts.
    pub hooks: Vec<Arc<dyn Hook>>,
    /// The bus, for tests and probes.
    pub bus: Arc<EventBus>,
    /// The reviews in flight, so a shutdown can wait for them to close.
    pub coordinator: Arc<Coordinator>,
}

/// Builds the server from the application. Secrets come from the
/// environment variables the settings name; a missing one disables its
/// routes with a clear status.
#[must_use]
pub fn compose(app: &Arc<App>) -> Composed {
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
    compose_with_secrets(app, github_secret, gitlab_token, api_token)
}

/// Builds the server with explicit secrets, for tests and for [`compose`].
#[must_use]
pub fn compose_with_secrets(
    app: &Arc<App>,
    github_secret: Option<SecretString>,
    gitlab_token: Option<SecretString>,
    api_token: Option<SecretString>,
) -> Composed {
    let settings = Arc::new(app.settings.clone());
    let writers: Arc<dyn Writers> = Arc::clone(app) as Arc<dyn Writers>;
    let coordinator = Arc::new(Coordinator::new(Arc::clone(app)));
    let bus = Arc::new(EventBus::new(
        Arc::new(StoreRecorder(Arc::clone(&app.store))),
        vec![
            Arc::new(ReviewListener::new(
                Arc::clone(&coordinator),
                Arc::clone(&writers),
            )),
            Arc::new(MentionListener::new(settings, writers)),
            Arc::new(PlanListener::new(Arc::clone(&coordinator))),
        ],
    ));
    let requester = app
        .settings
        .planning
        .as_ref()
        .map(|p| p.requester_id.to_string());
    let github = Arc::new(GitHubHook::new(github_secret, Arc::clone(&bus)));
    let gitlab = Arc::new(GitLabHook::new(gitlab_token, Arc::clone(&bus)));
    let api = Arc::new(ApiHook::new(api_token, requester, Arc::clone(&bus)));

    let shared = Arc::new(Shared {
        coordinator: Arc::clone(&coordinator),
        bus: Arc::clone(&bus),
    });
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/runs/{id}", get(run_page))
        .route("/events/{id}", get(event_page))
        .with_state(shared)
        .merge(Arc::clone(&github).routes())
        .merge(Arc::clone(&gitlab).routes())
        .merge(Arc::clone(&api).routes())
        .layer(TraceLayer::new_for_http());
    let hooks: Vec<Arc<dyn Hook>> = vec![github, gitlab, api];
    Composed {
        router,
        hooks,
        bus,
        coordinator,
    }
}

/// How long a shutdown waits for reviews in flight to close their checks.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Serves until SIGINT or SIGTERM. Hooks with long-running parts run
/// alongside and are cancelled on shutdown.
///
/// # Errors
///
/// Returns an error when the bind address is unusable.
pub async fn serve(app: Arc<App>) -> anyhow::Result<()> {
    crate::liveness::reap_orphans(&app).await;
    let bind = app.settings.server.bind.clone();
    let composed = compose(&app);
    let cancel = CancellationToken::new();
    let mut hook_tasks = tokio::task::JoinSet::new();
    for hook in composed.hooks {
        info!(hook = hook.name(), "hook ready");
        hook_tasks.spawn(hook.run(Arc::clone(&composed.bus), cancel.clone()));
    }
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    info!(%bind, listeners = ?composed.bus.listeners().collect::<Vec<_>>(), "listening");
    axum::serve(listener, composed.router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving")?;
    // Reviews in flight end as interrupted and close their checks (#7).
    app.shutdown.cancel();
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    while composed.coordinator.active_reviews() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    cancel.cancel();
    while hook_tasks.join_next().await.is_some() {}
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

async fn healthz(State(shared): State<Arc<Shared>>) -> Response {
    axum::Json(json!({
        "status": "ok",
        "active_reviews": shared.coordinator.active_reviews(),
        "listeners": shared.bus.listeners().collect::<Vec<_>>(),
    }))
    .into_response()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;max-width:60rem;margin:2rem auto;padding:0 1rem;color:#222}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:.3rem .6rem;border-bottom:1px solid #ddd;vertical-align:top}code{background:#f3f3f3;padding:0 .2rem}pre{background:#f3f3f3;padding:.6rem;overflow:auto;max-height:30rem}";

fn page(title: &str, body: &str) -> Response {
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        escape(title)
    ))
    .into_response()
}

async fn run_page(State(shared): State<Arc<Shared>>, Path(id): Path<String>) -> Response {
    let Ok(run_id) = RunId::parse(id) else {
        return (StatusCode::BAD_REQUEST, "bad run id").into_response();
    };
    let store = &shared.coordinator.app().store;
    let run = match store.run(&run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let lanes = store.lanes(&run_id).await.unwrap_or_default();
    let events = store.events(&run_id).await.unwrap_or_default();
    let inbound = store
        .inbound_events_for_run(&run_id)
        .await
        .unwrap_or_default();

    let mut html = String::new();
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
    if !inbound.is_empty() {
        html.push_str("<h2>Events</h2><table><tr><th>Event</th><th>Received</th><th>Source</th><th>Kind</th></tr>");
        for event in &inbound {
            let _ = write!(
                html,
                "<tr><td><a href=\"/events/{0}\">{0}</a></td><td>{1}</td><td>{2}</td><td>{3}</td></tr>",
                escape(event.id.as_str()),
                escape(&event.received_at),
                escape(&event.source),
                escape(&event.kind)
            );
        }
        html.push_str("</table>");
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
    page(&format!("Run {}", run.id), &html)
}

async fn event_page(State(shared): State<Arc<Shared>>, Path(id): Path<String>) -> Response {
    let Ok(event_id) = EventId::parse(id) else {
        return (StatusCode::BAD_REQUEST, "bad event id").into_response();
    };
    let store = &shared.coordinator.app().store;
    let event = match store.inbound_event(&event_id).await {
        Ok(Some(event)) => event,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such event").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let outcomes = store.outcomes(&event_id).await.unwrap_or_default();
    let mut html = String::new();
    let _ = write!(
        html,
        "<h1>Event {}</h1><p>Received {}<br>Source: <b>{}</b><br>Kind: <b>{}</b>{}</p>",
        escape(event.id.as_str()),
        escape(&event.received_at),
        escape(&event.source),
        escape(&event.kind),
        match (&event.repo, event.target) {
            (Some(repo), Some(target)) => format!("<br>About: {} #{target}", escape(repo)),
            (Some(repo), None) => format!("<br>About: {}", escape(repo)),
            _ => String::new(),
        }
    );
    html.push_str("<h2>What the listeners did</h2><table><tr><th>Listener</th><th>Outcome</th><th>Detail</th><th>Run</th><th>At</th></tr>");
    for outcome in &outcomes {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&outcome.listener),
            escape(&outcome.outcome),
            escape(&outcome.detail),
            outcome
                .run_id
                .as_deref()
                .map(|r| format!("<a href=\"/runs/{0}\">{0}</a>", escape(r)))
                .unwrap_or_default(),
            escape(&outcome.at)
        );
    }
    html.push_str("</table>");
    if let Some(payload) = &event.payload {
        let _ = write!(
            html,
            "<h2>Payload as received</h2><pre>{}</pre>",
            escape(payload)
        );
    }
    page(&format!("Event {}", event.id), &html)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use hmac::{Hmac, KeyInit as _, Mac as _};
    use http_body_util::BodyExt as _;
    use serde_json::Value;
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

    async fn test_app() -> Arc<App> {
        let settings = Config::parse(MINIMAL).unwrap().into_settings().unwrap();
        Arc::new(
            App::build(settings, Some(std::path::Path::new(":memory:")))
                .await
                .unwrap(),
        )
    }

    fn composed(app: &Arc<App>) -> Composed {
        compose_with_secrets(
            app,
            Some(SecretString::from("whsec".to_owned())),
            Some(SecretString::from("gltok".to_owned())),
            Some(SecretString::from("apitok".to_owned())),
        )
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        mac.finalize()
            .into_bytes()
            .iter()
            .fold("sha256=".to_owned(), |mut acc, b| {
                let _ = write!(acc, "{b:02x}");
                acc
            })
    }

    async fn call(router: Router, request: Request<Body>) -> (StatusCode, String) {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn wait_for_outcomes(app: &Arc<App>, event: &str) -> usize {
        let id = EventId::parse(event).unwrap();
        for _ in 0..100 {
            let n = app.store.outcomes(&id).await.map_or(0, |o| o.len());
            if n >= 3 {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        0
    }

    #[tokio::test]
    async fn healthz_reports_listeners() {
        let app = test_app().await;
        let (status, body) = call(
            composed(&app).router,
            Request::get("/healthz").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"active_reviews\":0"));
        assert!(body.contains("\"review\""));
    }

    #[tokio::test]
    async fn github_webhook_checks_the_signature_and_records_the_event() {
        let app = test_app().await;
        let payload = br#"{"zen":"keep it simple","repository":{"full_name":"docspec/app"},"sender":{"login":"a","type":"User"}}"#;
        let bad = Request::post("/webhooks/github")
            .header("x-github-event", "ping")
            .header("x-hub-signature-256", "sha256=00")
            .body(Body::from(payload.to_vec()))
            .unwrap();
        assert_eq!(
            call(composed(&app).router, bad).await.0,
            StatusCode::UNAUTHORIZED
        );

        let good = Request::post("/webhooks/github")
            .header("x-github-event", "ping")
            .header("x-github-delivery", "d-1")
            .header("x-hub-signature-256", sign("whsec", payload))
            .body(Body::from(payload.to_vec()))
            .unwrap();
        let (status, body) = call(composed(&app).router, good).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let event = serde_json::from_str::<Value>(&body).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            wait_for_outcomes(&app, &event).await,
            3,
            "every listener recorded an outcome"
        );
        let recorded = app
            .store
            .inbound_event(&EventId::parse(event.clone()).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recorded.kind, "unmodelled");
        assert_eq!(recorded.source, "github_webhook");
        assert!(
            recorded
                .payload
                .as_deref()
                .unwrap_or("")
                .contains("keep it simple")
        );

        let (status, page) = call(
            composed(&app).router,
            Request::get(format!("/events/{event}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("unmodelled") && page.contains("keep it simple"));
    }

    #[tokio::test]
    async fn gitlab_webhook_checks_the_token() {
        let app = test_app().await;
        let payload = br#"{"object_kind":"push","project":{"path_with_namespace":"9xxlab/app"}}"#;
        let bad = Request::post("/webhooks/gitlab")
            .header("x-gitlab-token", "nope")
            .body(Body::from(payload.to_vec()))
            .unwrap();
        assert_eq!(
            call(composed(&app).router, bad).await.0,
            StatusCode::UNAUTHORIZED
        );
        let good = Request::post("/webhooks/gitlab")
            .header("x-gitlab-token", "gltok")
            .body(Body::from(payload.to_vec()))
            .unwrap();
        assert_eq!(
            call(composed(&app).router, good).await.0,
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn api_publishes_requests_as_events() {
        let app = test_app().await;
        let no_token = Request::post("/review")
            .body(Body::from(
                r#"{"url":"https://github.com/docspec/app/pull/1"}"#,
            ))
            .unwrap();
        assert_eq!(
            call(composed(&app).router, no_token).await.0,
            StatusCode::UNAUTHORIZED
        );
        let bad_url = Request::post("/plan")
            .header("authorization", "Bearer apitok")
            .body(Body::from(r#"{"url":"https://example.com/x"}"#))
            .unwrap();
        assert_eq!(
            call(composed(&app).router, bad_url).await.0,
            StatusCode::BAD_REQUEST
        );

        let good = Request::post("/plan")
            .header("authorization", "Bearer apitok")
            .body(Body::from(
                r#"{"url":"https://github.com/docspec/app/issues/9","note":"small"}"#,
            ))
            .unwrap();
        let (status, body) = call(composed(&app).router, good).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let event = serde_json::from_str::<Value>(&body).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(wait_for_outcomes(&app, &event).await, 3);
        let recorded = app
            .store
            .inbound_event(&EventId::parse(event).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recorded.kind, "plan_requested");
        assert_eq!(recorded.source, "api");
    }

    #[tokio::test]
    async fn unknown_runs_and_events_are_not_found() {
        let app = test_app().await;
        let (status, _) = call(
            composed(&app).router,
            Request::get("/runs/r-nope").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            composed(&app).router,
            Request::get("/events/e-nope").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            composed(&app).router,
            Request::get("/runs/bad%20id").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
