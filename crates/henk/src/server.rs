//! The HTTP server: composition of hooks, listeners and the bus, plus the
//! pages that make runs and events traceable (§8.6).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use henk_domain::run::{EventId, RunId};
use henk_events::{EventBus, Hook};
use secrecy::SecretString;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tower_http::trace::{DefaultOnRequest, OnRequest, TraceLayer};
use tracing::{info, warn};

use crate::app::{App, env_var};
use crate::coordinator::Coordinator;
use crate::dashboard::{self, Dashboard, DashboardSecrets};
use crate::hooks::{ApiHook, GitHubHook, GitLabHook, HttpHook};
use crate::listeners::{AddressListener, MentionListener, PlanListener, ReviewListener, Writers};
use crate::pages;
use crate::recorder::StoreRecorder;

/// What the server's own pages can reach.
pub struct Shared {
    coordinator: Arc<Coordinator>,
    bus: Arc<EventBus>,
    /// Whether the dashboard is up. Run and event pages are served only
    /// there, behind sign-in (#69); without it they are not served.
    dashboard: AtomicBool,
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
    /// State of the server's own routes.
    shared: Arc<Shared>,
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
    let mut composed = compose_with_secrets(app, github_secret, gitlab_token, api_token);
    if let Some(config) = &app.settings.dashboard {
        let built = DashboardSecrets::from_env(config)
            .map_err(anyhow::Error::msg)
            .and_then(|secrets| {
                Dashboard::new(
                    Arc::clone(app),
                    Arc::clone(&composed.coordinator),
                    Arc::clone(&composed.bus),
                    config.clone(),
                    &secrets,
                )
            });
        match built {
            Ok(dashboard) => {
                composed.router = with_dashboard(
                    composed.router,
                    dashboard::routes(Arc::new(dashboard)),
                    DefaultOnRequest::default(),
                );
                composed.shared.dashboard.store(true, Ordering::Release);
                info!("dashboard at /dashboard");
            }
            Err(error) => warn!(%error, "dashboard disabled"),
        }
    }
    composed
}

/// Adds the dashboard's routes to a composed router. The composed routes
/// are already traced, so only the dashboard's own routes get a trace
/// layer here; layering the merged router would trace the rest twice.
/// `on_request` is the trace layer's request hook: [`compose`] passes the
/// default one, a test passes one that counts (#148).
fn with_dashboard<O>(router: Router, dashboard: Router, on_request: O) -> Router
where
    O: OnRequest<Body> + Clone + Send + Sync + 'static,
{
    router.merge(dashboard.layer(TraceLayer::new_for_http().on_request(on_request)))
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
            Arc::new(AddressListener::new(Arc::clone(&coordinator))),
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
        dashboard: AtomicBool::new(false),
    });
    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/runs/{id}", get(run_page))
        .route("/events/{id}", get(event_page))
        .with_state(Arc::clone(&shared))
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
        shared,
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
    let bind = app.settings.server.bind.clone();
    let composed = compose(&app);
    let cancel = CancellationToken::new();
    // Reaps at once, then every minute: a crash followed by a restart
    // within the staleness window is closed too (#47).
    let reaper = crate::liveness::spawn_reaper(Arc::clone(&app), cancel.clone());
    // Workspaces a process that died left on the sandbox host go now (#84).
    // The sweep starts here, before any run: a workspace opened meanwhile
    // waits for it instead of being swept.
    if let Some(sweep) = app.workspace_provider.sweep() {
        tokio::spawn(async move {
            match sweep.await {
                Ok(()) => info!("sandbox host swept"),
                Err(error) => warn!(%error, "could not sweep the sandbox host"),
            }
        });
    }
    // Events older than server.keep_events_days go; runs stay (#69).
    let pruner = crate::prune::spawn_pruner(Arc::clone(&app), cancel.clone());
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
    while composed.coordinator.tracked_reviews() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    cancel.cancel();
    while hook_tasks.join_next().await.is_some() {}
    let _ = reaper.await;
    let _ = pruner.await;
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
        "active_reviews": shared.coordinator.running_reviews(),
        "queued_reviews": shared.coordinator.queued_reviews(),
        "listeners": shared.bus.listeners().collect::<Vec<_>>(),
    }))
    .into_response()
}

/// A run link as posted on a pull request or issue: the run is shown on
/// the dashboard, behind sign-in (#69). Findings, plans and events of
/// private repositories are never served to whoever has the link.
async fn run_page(State(shared): State<Arc<Shared>>, Path(id): Path<String>) -> Response {
    let Ok(run_id) = RunId::parse(id) else {
        return (StatusCode::BAD_REQUEST, "bad run id").into_response();
    };
    signed_in_page(&shared, &format!("/dashboard/runs/{run_id}"))
}

/// An event link: shown on the dashboard, behind sign-in (#69).
async fn event_page(State(shared): State<Arc<Shared>>, Path(id): Path<String>) -> Response {
    let Ok(event_id) = EventId::parse(id) else {
        return (StatusCode::BAD_REQUEST, "bad event id").into_response();
    };
    signed_in_page(&shared, &format!("/dashboard/events/{event_id}"))
}

/// Sends the browser to the dashboard page, or explains that there is none.
fn signed_in_page(shared: &Shared, to: &str) -> Response {
    if !shared.dashboard.load(Ordering::Acquire) {
        return (
            StatusCode::NOT_FOUND,
            pages::page(
                "Not served here",
                "",
                "<h1>Not served here</h1><p>Run and event pages are shown on the dashboard, \
                 behind sign-in. This Henk has no dashboard: add <code>[dashboard]</code> to \
                 henk.toml. <code>henk runs show</code> reads a run on the server.</p>",
            ),
        )
            .into_response();
    }
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = axum::http::HeaderValue::from_str(to) {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, location);
    }
    response
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::fmt::Write as _;
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
            if n >= 4 {
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
        assert!(body.contains("\"queued_reviews\":0"));
        assert!(body.contains("\"review\""));
    }

    /// The dashboard's trace layer sees its own routes once and the
    /// composed routes not at all: layering the merged router would trace
    /// every other route twice. Counted through the layer's request hook,
    /// not from log lines, since whether `tracing` logs a call site is
    /// cached for the whole process and other tests running alongside
    /// could hide a line (#148).
    #[tokio::test]
    async fn the_dashboard_does_not_trace_the_other_routes_twice() {
        use std::sync::atomic::AtomicUsize;

        let app = test_app().await;
        let dashboard = Router::new().route("/dashboard/x", get(|| async { "ok" }));
        let traced = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&traced);
        let router = with_dashboard(
            composed(&app).router,
            dashboard,
            move |_: &Request<Body>, _: &tracing::Span| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        );
        for (path, layered) in [("/healthz", 0), ("/dashboard/x", 1)] {
            let before = traced.load(Ordering::SeqCst);
            let (status, _) = call(
                router.clone(),
                Request::get(path).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(
                traced.load(Ordering::SeqCst) - before,
                layered,
                "{path} passes the dashboard's trace layer {layered} time(s)"
            );
        }
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
            4,
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
        assert_eq!(status, StatusCode::NOT_FOUND, "no dashboard, no event page");
        assert!(
            !page.contains("keep it simple"),
            "a recorded payload is never served without sign-in"
        );
    }

    #[tokio::test]
    async fn run_and_event_links_lead_to_the_dashboard() {
        let app = test_app().await;
        let with = composed(&app);
        with.shared.dashboard.store(true, Ordering::Release);
        for (link, to) in [
            ("/runs/r-1", "/dashboard/runs/r-1"),
            ("/events/e-1", "/dashboard/events/e-1"),
        ] {
            let response = with
                .router
                .clone()
                .oneshot(Request::get(link).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{link}");
            assert_eq!(response.headers()[axum::http::header::LOCATION], to);
        }
        let (status, page) = call(
            composed(&app).router,
            Request::get("/runs/r-1").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(page.contains("[dashboard]"), "{page}");
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
        assert_eq!(wait_for_outcomes(&app, &event).await, 4);
        let recorded = app
            .store
            .inbound_event(&EventId::parse(event).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recorded.kind, "plan_requested");
        assert_eq!(recorded.source, "api");

        let address = Request::post("/address")
            .header("authorization", "Bearer apitok")
            .body(Body::from(
                r#"{"url":"https://github.com/docspec/app/pull/7","note":"only the typo"}"#,
            ))
            .unwrap();
        let (status, body) = call(composed(&app).router, address).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let event = serde_json::from_str::<Value>(&body).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(wait_for_outcomes(&app, &event).await, 4);
        let id = EventId::parse(event).unwrap();
        let recorded = app.store.inbound_event(&id).await.unwrap().unwrap();
        assert_eq!(recorded.kind, "address_requested");
        let outcome = app
            .store
            .outcomes(&id)
            .await
            .unwrap()
            .into_iter()
            .find(|o| o.listener == "address")
            .unwrap();
        assert_eq!(outcome.outcome, "ignored");
        assert_eq!(outcome.detail, "address runs are not configured");

        let gitlab = Request::post("/address")
            .header("authorization", "Bearer apitok")
            .body(Body::from(
                r#"{"url":"https://gitlab.com/9xxlab/tools/cli/-/merge_requests/5"}"#,
            ))
            .unwrap();
        let (status, body) = call(composed(&app).router, gitlab).await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "a merge request is addressed too"
        );
        let event = serde_json::from_str::<Value>(&body).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(wait_for_outcomes(&app, &event).await, 4);
        let recorded = app
            .store
            .inbound_event(&EventId::parse(event).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recorded.kind, "address_requested");
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
        let (status, _) = call(
            compose(&app).router,
            Request::get("/dashboard").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "no [dashboard], no dashboard"
        );
    }
}
