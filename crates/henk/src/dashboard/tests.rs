#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use henk_store::{FindingAction, InboundEvent, LaneStatus, NewRun, OutcomeRecord, RunStatus};
use http_body_util::BodyExt as _;
use serde_json::json;
use tower::ServiceExt as _;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::session::{SESSION_COOKIE, STATE_COOKIE, Session, cookie_value};
use super::*;
use crate::config::Config;

pub(super) const ALLOWED: u64 = 1234;

fn config(github: &str) -> String {
    format!(
        r#"
[server]
public_base_url = "https://henk.example"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
[dashboard]
allowed_github_ids = [{ALLOWED}]
github_web_base = "{github}"
github_api_base = "{github}"
"#
    )
}

pub(super) struct Fixture {
    pub(super) router: Router,
    pub(super) dashboard: Arc<Dashboard>,
}

pub(super) fn fixture(github: &str) -> Fixture {
    fixture_with_assets(github, app::Assets(&[]))
}

/// A fixture whose dashboard app is `assets`.
pub(super) fn fixture_with_assets(github: &str, assets: app::Assets) -> Fixture {
    fixture_with(github, assets, |_| {})
}

/// A fixture whose dashboard app is `assets`, with `edit` applied to the
/// settings.
pub(super) fn fixture_with(
    github: &str,
    assets: app::Assets,
    edit: impl FnOnce(&mut crate::config::Settings),
) -> Fixture {
    let mut settings = Config::parse(&config(github))
        .and_then(Config::into_settings)
        .unwrap_or_else(|e| panic!("{e}"));
    edit(&mut settings);
    let dashboard_config = settings.dashboard.clone().unwrap();
    // The store announces its writes, as Henk's own does (#202).
    let feed = crate::live::Feed::default();
    let app = Arc::new(App {
        settings,
        store: Arc::new(crate::live::Announcing::new(
            Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
            feed.clone(),
        )),
        models: BTreeMap::new(),
        github: None,
        gitlab: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        live_runs: crate::liveness::LiveRuns::default(),
        feed,
        cancels: crate::cancel::Cancels::default(),
        workspace_provider: Arc::new(crate::workspace::host::HostProvider),
        test_writer: None,
        test_session: None,
        test_address_writer: None,
        test_issue_writer: None,
    });
    let coordinator = Arc::new(Coordinator::new(Arc::clone(&app)));
    let writers: Arc<dyn crate::listeners::Writers> =
        Arc::clone(&app) as Arc<dyn crate::listeners::Writers>;
    let bus = Arc::new(henk_events::EventBus::new(
        Arc::new(crate::recorder::StoreRecorder(Arc::clone(&app.store))),
        vec![
            Arc::new(crate::listeners::ReviewListener::new(
                Arc::clone(&coordinator),
                writers,
            )),
            Arc::new(crate::listeners::PlanListener::new(Arc::clone(
                &coordinator,
            ))),
            Arc::new(crate::listeners::AddressListener::new(Arc::clone(
                &coordinator,
            ))),
        ],
    ));
    let secrets = DashboardSecrets {
        client_id: "cid".to_owned(),
        client_secret: SecretString::from("csecret".to_owned()),
        session_key: SecretString::from("k".repeat(32)),
    };
    let mut dashboard = Dashboard::new(app, coordinator, bus, dashboard_config, &secrets).unwrap();
    dashboard.assets = assets;
    dashboard.refresh = Duration::from_millis(50);
    let dashboard = Arc::new(dashboard);
    Fixture {
        router: routes(Arc::clone(&dashboard)),
        dashboard,
    }
}

fn signed_in(f: &Fixture, github_id: u64) -> String {
    signed_in_as(f, &Session::fresh(github_id, "alice".to_owned()).unwrap()).0
}

/// The session cookie and the CSRF token of `session`.
pub(super) fn signed_in_as(f: &Fixture, session: &Session) -> (String, String) {
    (
        format!(
            "{SESSION_COOKIE}={}",
            f.dashboard.signer.session(session, Duration::from_hours(1))
        ),
        f.dashboard.signer.csrf(session),
    )
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn get(f: &Fixture, uri: &str, cookie: Option<&str>) -> Answer {
    let mut request = Request::get(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = f
        .router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    Answer {
        status,
        headers,
        body,
    }
}

fn set_cookies(answer: &Answer) -> Vec<String> {
    answer
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

pub(super) async fn seed(f: &Fixture) {
    let store = &f.dashboard.app.store;
    let run = |id: &str, kind: RunKind, trigger: &str| NewRun {
        id: RunId::parse(id).unwrap(),
        kind,
        platform: Platform::GitHub,
        repo: "docspec/app".into(),
        target: 7,
        commit: None,
        requester: None,
        trigger: trigger.into(),
        link: format!("https://henk.example/runs/{id}"),
    };
    store
        .create_run(&run("r-review", RunKind::Review, "opened"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    store
        .create_run(&run("r-plan", RunKind::Plan, "<script>alert(1)</script>"))
        .await
        .unwrap();
    store
        .finish_run(
            &RunId::parse("r-plan").unwrap(),
            RunStatus::Finished,
            Some("plan written"),
            None,
        )
        .await
        .unwrap();
    let review = RunId::parse("r-review").unwrap();
    store
        .start_lane(&review, "lane-a", "model-x")
        .await
        .unwrap();
    store
        .finish_lane(&review, "lane-a", LaneStatus::Finished, 3, 100, 20, None)
        .await
        .unwrap();
    store
        .record_finding(
            &review,
            "lane-a",
            "src/a.rs",
            4,
            "c-77",
            FindingAction::Posted,
        )
        .await
        .unwrap();
    store
        .record_event(&InboundEvent {
            id: EventId::parse("e-1").unwrap(),
            received_at: "2026-10-06T00:00:00Z".into(),
            source: "github_webhook".into(),
            kind: "pull_request".into(),
            repo: Some("docspec/app".into()),
            target: Some(7),
            payload: Some("{\"title\": \"<img src=x onerror=alert(1)>\"}".into()),
            requester: None,
        })
        .await
        .unwrap();
    store
        .record_outcome(&OutcomeRecord {
            event_id: EventId::parse("e-1").unwrap(),
            listener: "review".into(),
            outcome: "started".into(),
            detail: "r-review".into(),
            run_id: Some("r-review".into()),
            at: String::new(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn nothing_shows_without_a_session() {
    let f = fixture("https://127.0.0.1:9");
    for (uri, login) in [
        ("/dashboard", "/dashboard/login"),
        (
            "/dashboard/events",
            "/dashboard/login?next=%2Fdashboard%2Fevents",
        ),
        (
            "/dashboard/health",
            "/dashboard/login?next=%2Fdashboard%2Fhealth",
        ),
        (
            "/dashboard/runs/r-1",
            "/dashboard/login?next=%2Fdashboard%2Fruns%2Fr-1",
        ),
    ] {
        let answer = get(&f, uri, None).await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(answer.headers[header::LOCATION], login, "{uri}");
    }
    let forged = format!("{SESSION_COOKIE}=bm90LXNpZ25lZA.AAAA");
    assert_eq!(
        get(&f, "/dashboard", Some(&forged)).await.status,
        StatusCode::SEE_OTHER
    );
    let removed = signed_in(&f, 999);
    assert_eq!(
        get(&f, "/dashboard", Some(&removed)).await.status,
        StatusCode::SEE_OTHER,
        "a genuine session for an id not on the list gets nothing"
    );
}

#[tokio::test]
async fn signing_in_through_github_lets_only_listed_ids_in() {
    let github = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_partial_json(
            json!({"client_id": "cid", "client_secret": "csecret", "code": "good"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "gho_secret_token"})),
        )
        .mount(&github)
        .await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(body_partial_json(json!({"code": "stranger"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "gho_other"})),
        )
        .mount(&github)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer gho_secret_token",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": ALLOWED, "login": "alice"})),
        )
        .mount(&github)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer gho_other",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": 999, "login": "mallory"})),
        )
        .mount(&github)
        .await;
    let base = github.uri().replace("localhost", "127.0.0.1");
    let f = fixture(&base);

    let login = get(&f, "/dashboard/login", None).await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    let to = login.headers[header::LOCATION].to_str().unwrap().to_owned();
    assert!(to.starts_with(&format!("{base}/login/oauth/authorize?client_id=cid&redirect_uri=https%3A%2F%2Fhenk.example%2Fdashboard%2Fauth%2Fcallback&scope=&state=")), "{to}");
    let state = to
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let state_cookie = set_cookies(&login)
        .into_iter()
        .find(|c| c.starts_with(STATE_COOKIE))
        .unwrap();
    let state_cookie = state_cookie.split(';').next().unwrap().to_owned();

    let wrong = get(
        &f,
        "/dashboard/auth/callback?code=good&state=forged",
        Some(&state_cookie),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    assert!(
        wrong.body.contains("<h1>Sign-in failed</h1>"),
        "{}",
        wrong.body
    );
    assert!(
        wrong
            .body
            .contains("href=\"/dashboard/login\">Sign in with GitHub</a>"),
        "a failed sign-in offers a fresh one: {}",
        wrong.body
    );
    let no_cookie = get(
        &f,
        &format!("/dashboard/auth/callback?code=good&state={state}"),
        None,
    )
    .await;
    assert_eq!(no_cookie.status, StatusCode::BAD_REQUEST);

    let stranger = get(
        &f,
        &format!("/dashboard/auth/callback?code=stranger&state={state}"),
        Some(&state_cookie),
    )
    .await;
    assert_eq!(stranger.status, StatusCode::FORBIDDEN);
    assert!(
        stranger.body.contains("<h1>Not for you</h1>"),
        "{}",
        stranger.body
    );
    assert!(
        stranger
            .body
            .contains("Signed in at GitHub as github:999. Ask an operator to add this id."),
        "the refusal names the id to add: {}",
        stranger.body
    );
    assert!(
        !stranger.body.contains("mallory"),
        "the id, never the login (2)"
    );
    assert!(
        stranger
            .body
            .contains("href=\"/dashboard/login\">Sign in again</a>")
    );
    assert!(!stranger.body.contains("<script"));
    assert!(
        stranger.headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    assert!(
        set_cookies(&stranger)
            .iter()
            .all(|c| !c.starts_with(&format!("{SESSION_COOKIE}=")))
    );

    let good = get(
        &f,
        &format!("/dashboard/auth/callback?code=good&state={state}"),
        Some(&state_cookie),
    )
    .await;
    assert_eq!(good.status, StatusCode::SEE_OTHER);
    assert_eq!(good.headers[header::LOCATION], "/dashboard");
    let cookies = set_cookies(&good);
    let session = cookies
        .iter()
        .find(|c| c.starts_with(&format!("{SESSION_COOKIE}=")))
        .unwrap();
    assert!(
        session.contains("HttpOnly")
            && session.contains("Secure")
            && session.contains("Path=/dashboard"),
        "{session}"
    );
    for cookie in &cookies {
        assert!(
            !cookie.contains("gho_secret_token"),
            "the GitHub token is never handed out"
        );
    }
    let value = cookie_value(session.split(';').next().unwrap(), SESSION_COOKIE).unwrap();
    assert_eq!(
        f.dashboard.signer.read_session(value).unwrap().github_id,
        ALLOWED
    );
}

/// Signs in through the mocked GitHub, starting at `login`, and returns
/// where the callback sends the browser.
async fn sign_in_from(f: &Fixture, login: &str) -> String {
    let started = get(f, login, None).await;
    assert_eq!(started.status, StatusCode::SEE_OTHER, "{login}");
    let to = started.headers[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let state = to
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_owned();
    let cookie = set_cookies(&started)
        .into_iter()
        .find(|c| c.starts_with(STATE_COOKIE))
        .unwrap();
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let back = get(
        f,
        &format!("/dashboard/auth/callback?code=good&state={state}"),
        Some(&cookie),
    )
    .await;
    assert_eq!(back.status, StatusCode::SEE_OTHER, "{login}");
    back.headers[header::LOCATION].to_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_run_link_leads_through_sign_in_back_to_the_run() {
    let github = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "gho_t"})))
        .mount(&github)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id": ALLOWED, "login": "alice"})),
        )
        .mount(&github)
        .await;
    let f = fixture(&github.uri().replace("localhost", "127.0.0.1"));

    let unsigned = get(&f, "/dashboard/runs/r-1", None).await;
    assert_eq!(unsigned.status, StatusCode::SEE_OTHER);
    assert_eq!(
        unsigned.headers[header::LOCATION],
        "/dashboard/login?next=%2Fdashboard%2Fruns%2Fr-1"
    );
    assert_eq!(
        sign_in_from(&f, "/dashboard/login?next=%2Fdashboard%2Fruns%2Fr-1").await,
        "/dashboard/runs/r-1"
    );
    for hostile in [
        "https%3A%2F%2Fevil.example%2F",
        "%2F%2Fevil.example",
        "%2Fdashboardevil",
        "%2Fdashboard%2F%5C%5Cevil.example",
    ] {
        assert_eq!(
            sign_in_from(&f, &format!("/dashboard/login?next={hostile}")).await,
            "/dashboard",
            "{hostile}"
        );
    }
    assert_eq!(sign_in_from(&f, "/dashboard/login").await, "/dashboard");
}

/// A refused action: what is wrong with it, the headers sent, the form.
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, String);

/// The dashboard's own origin, as `public_base_url` in [`config`] says.
const ORIGIN: &str = "https://henk.example";

/// A form POST with `cookie` and the given extra headers.
async fn post(
    f: &Fixture,
    uri: &str,
    cookie: &str,
    headers: &[(&str, &str)],
    form: &str,
) -> Answer {
    let mut request = Request::post(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = f
        .router
        .clone()
        .oneshot(request.body(Body::from(form.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// The outcomes of `event` once every listener has answered.
pub(super) async fn outcomes_of(
    f: &Fixture,
    event: &EventId,
    listeners: usize,
) -> Vec<OutcomeRecord> {
    for _ in 0..200 {
        let outcomes = f.dashboard.app.store.outcomes(event).await.unwrap();
        if outcomes.len() >= listeners {
            return outcomes;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the listeners did not answer {event}");
}

#[tokio::test]
async fn signing_out_needs_the_token_and_the_dashboards_own_origin() {
    let f = fixture("https://127.0.0.1:9");
    let session = Session::fresh(ALLOWED, "alice".to_owned()).unwrap();
    let (cookie, csrf) = signed_in_as(&f, &session);
    let (_, other_csrf) = signed_in_as(&f, &Session::fresh(ALLOWED, "alice".to_owned()).unwrap());
    let form = |token: &str| format!("csrf={token}");
    let cases: Vec<Case> = vec![
        ("no token", vec![("origin", ORIGIN)], String::new()),
        (
            "another session's token",
            vec![("origin", ORIGIN)],
            form(&other_csrf),
        ),
        ("a forged token", vec![("origin", ORIGIN)], form("AAAA")),
        (
            "another origin",
            vec![("origin", "https://evil.example")],
            form(&csrf),
        ),
        (
            "a look-alike origin",
            vec![("origin", "https://henk.example.evil.com")],
            form(&csrf),
        ),
        ("a null origin", vec![("origin", "null")], form(&csrf)),
        (
            "another referer",
            vec![("referer", "https://evil.example/dashboard")],
            form(&csrf),
        ),
        (
            "a look-alike referer",
            vec![("referer", "https://henk.example.evil.com/")],
            form(&csrf),
        ),
        ("neither origin nor referer", vec![], form(&csrf)),
    ];
    for (what, headers, body) in cases {
        let answer = post(&f, "/dashboard/logout", &cookie, &headers, &body).await;
        assert_eq!(
            answer.status,
            StatusCode::FORBIDDEN,
            "{what}: {}",
            answer.body
        );
        assert!(set_cookies(&answer).is_empty(), "{what}: still signed in");
    }
    let removed = signed_in(&f, 999);
    let answer = post(
        &f,
        "/dashboard/logout",
        &removed,
        &[("origin", ORIGIN)],
        &form(&csrf),
    )
    .await;
    assert_eq!(
        answer.status,
        StatusCode::FORBIDDEN,
        "an id not on the list"
    );

    let by_referer = post(
        &f,
        "/dashboard/logout",
        &cookie,
        &[("referer", "https://henk.example/dashboard/runs/r-1")],
        &form(&csrf),
    )
    .await;
    assert_eq!(
        by_referer.status,
        StatusCode::OK,
        "the dashboard's own referer"
    );
    let cleared = set_cookies(&by_referer);
    assert!(
        cleared
            .iter()
            .any(|c| c.starts_with(&format!("{SESSION_COOKIE}=;")) && c.contains("Max-Age=0")),
        "{cleared:?}"
    );
}

#[test]
fn a_github_app_client_id_is_told_from_an_oauth_app_one() {
    assert!(super::is_github_app_client_id("Iv1.0123456789abcdef"));
    assert!(super::is_github_app_client_id("Iv23liAbCdEfGhIjKlMn"));
    assert!(!super::is_github_app_client_id("Ov23liAbCdEfGhIjKlMn"));
    assert!(!super::is_github_app_client_id("0123456789abcdef0123"));
    assert!(!super::is_github_app_client_id("cid"));
}
