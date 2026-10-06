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
use henk_store::{FindingAction, InboundEvent, LaneStatus, NewRun, OutcomeRecord, Page, RunStatus};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::session::{SESSION_COOKIE, STATE_COOKIE, Session, cookie_value};
use super::*;
use crate::config::Config;

const ALLOWED: u64 = 1234;

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

struct Fixture {
    router: Router,
    dashboard: Arc<Dashboard>,
}

fn fixture(github: &str) -> Fixture {
    let settings = Config::parse(&config(github))
        .and_then(Config::into_settings)
        .unwrap_or_else(|e| panic!("{e}"));
    let dashboard_config = settings.dashboard.clone().unwrap();
    let app = Arc::new(App {
        settings,
        store: Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
        models: BTreeMap::new(),
        github: None,
        gitlab: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        test_writer: None,
        test_session: None,
        test_address_writer: None,
    });
    let coordinator = Arc::new(Coordinator::new(Arc::clone(&app)));
    let secrets = DashboardSecrets {
        client_id: "cid".to_owned(),
        client_secret: SecretString::from("csecret".to_owned()),
        session_key: SecretString::from("k".repeat(32)),
    };
    let dashboard = Arc::new(
        Dashboard::new(
            app,
            coordinator,
            vec!["review", "plan"],
            dashboard_config,
            &secrets,
        )
        .unwrap(),
    );
    Fixture {
        router: routes(Arc::clone(&dashboard)),
        dashboard,
    }
}

fn signed_in(f: &Fixture, github_id: u64) -> String {
    let session = Session {
        github_id,
        login: "alice".to_owned(),
    };
    format!(
        "{SESSION_COOKIE}={}",
        f.dashboard
            .signer
            .session(&session, Duration::from_hours(1))
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

async fn seed(f: &Fixture) {
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
    for uri in [
        "/dashboard",
        "/dashboard/events",
        "/dashboard/health",
        "/dashboard/runs/r-1",
    ] {
        let answer = get(&f, uri, None).await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(
            answer.headers[header::LOCATION],
            "/dashboard/login",
            "{uri}"
        );
    }
    assert_eq!(
        get(&f, "/dashboard/running.json", None).await.status,
        StatusCode::UNAUTHORIZED
    );
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
    assert!(to.starts_with(&format!("{base}/login/oauth/authorize?client_id=cid&redirect_uri=https%3A%2F%2Fhenk.example%2Fdashboard%2Fauth%2Fcallback&state=")), "{to}");
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

#[tokio::test]
async fn the_overview_lists_runs_by_filter_and_escapes_what_it_shows() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let me = signed_in(&f, ALLOWED);

    let all = get(&f, "/dashboard", Some(&me)).await;
    assert_eq!(all.status, StatusCode::OK);
    assert!(all.body.contains("Signed in as alice."));
    assert!(
        all.body
            .contains("<a href=\"/dashboard/runs/r-review\">r-review</a>"),
        "{}",
        all.body
    );
    assert!(
        all.body
            .contains("<a href=\"https://github.com/docspec/app/pull/7\">"),
        "{}",
        all.body
    );
    assert!(
        all.body
            .contains("<a href=\"https://github.com/docspec/app/issues/7\">"),
        "a plan links its issue"
    );
    assert!(all.body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(
        !all.body.contains("<script>alert"),
        "stored text never becomes markup"
    );
    assert!(
        all.body
            .contains("Running now (<span id=\"running-count\">1</span>)")
    );
    assert!(
        all.body
            .contains("<p class=\"muted\" id=\"running-more\" hidden></p>"),
        "nothing is cut off, so no \"more\" line"
    );
    assert!(
        henk_domain::text::is_in_style(&all.body),
        "page text is in style"
    );
    let csp = all.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("script-src 'self'"), "{csp}");
    assert_eq!(all.headers[header::X_FRAME_OPTIONS], "DENY");

    let plans = get(&f, "/dashboard?kind=plan", Some(&me)).await;
    let runs_section = plans.body.split("<h2>Runs</h2>").nth(1).unwrap();
    assert!(runs_section.contains("r-plan"));
    assert!(!runs_section.contains("r-review"));
    assert!(plans.body.contains("<option selected>plan</option>"));
    let hostile = get(&f, "/dashboard?repo=%22%3E%3Cscript%3E", Some(&me)).await;
    assert!(
        hostile.body.contains("value=\"&quot;&gt;&lt;script&gt;\""),
        "a filter value is escaped"
    );
}

#[tokio::test]
async fn paging_keeps_the_filters() {
    let f = fixture("https://127.0.0.1:9");
    for n in 0..51 {
        f.dashboard
            .app
            .store
            .create_run(&NewRun {
                id: RunId::parse(format!("r-{n:03}")).unwrap(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "docspec/app".into(),
                target: n,
                commit: None,
                requester: None,
                trigger: "opened".into(),
                link: String::new(),
            })
            .await
            .unwrap();
    }
    let me = signed_in(&f, ALLOWED);
    let first = get(&f, "/dashboard?kind=review", Some(&me)).await;
    assert!(
        first
            .body
            .contains("<a href=\"/dashboard?kind=review&page=1\">Older</a>"),
        "{}",
        first.body
    );
    assert!(!first.body.contains("Newer"));
    let second = get(&f, "/dashboard?kind=review&page=1", Some(&me)).await;
    let runs_section = second.body.split("<h2>Runs</h2>").nth(1).unwrap();
    assert_eq!(
        runs_section
            .matches("<tr><td><a href=\"/dashboard/runs/")
            .count(),
        1
    );
    assert!(second.body.contains("Newer"));
    assert!(!second.body.contains("Older"));
}

#[tokio::test]
async fn running_now_counts_every_running_run_beyond_one_page() {
    let f = fixture("https://127.0.0.1:9");
    let running = u64::from(Page::MAX) + 2;
    for n in 0..running {
        f.dashboard
            .app
            .store
            .create_run(&NewRun {
                id: RunId::parse(format!("r-{n:03}")).unwrap(),
                kind: RunKind::Plan,
                platform: Platform::GitHub,
                repo: "docspec/app".into(),
                target: n,
                commit: None,
                requester: None,
                trigger: "asked".into(),
                link: String::new(),
            })
            .await
            .unwrap();
    }
    let me = signed_in(&f, ALLOWED);

    let overview = get(&f, "/dashboard", Some(&me)).await;
    assert!(
        overview.body.contains(&format!(
            "Running now (<span id=\"running-count\">{running}</span>)"
        )),
        "the badge is the real count, not the page size"
    );
    assert!(
        overview
            .body
            .contains("<p class=\"muted\" id=\"running-more\">And 2 more not shown.</p>"),
        "{}",
        overview.body
    );
    let shown = overview
        .body
        .split("<h2>Runs</h2>")
        .next()
        .unwrap()
        .matches("<tr><td><a href=\"/dashboard/runs/")
        .count();
    assert_eq!(shown, usize::try_from(Page::MAX).unwrap());

    let json = get(&f, "/dashboard/running.json", Some(&me)).await;
    let json: Value = serde_json::from_str(&json.body).unwrap();
    assert_eq!(json["total"], running);
    assert_eq!(
        json["runs"].as_array().unwrap().len(),
        usize::try_from(Page::MAX).unwrap()
    );

    let script = get(&f, "/dashboard/app.js", Some(&me)).await;
    assert!(script.body.contains("running.total"));
    assert!(script.body.contains("more not shown."));
}

#[tokio::test]
async fn run_detail_events_health_and_the_poller_answer() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let me = signed_in(&f, ALLOWED);

    let run = get(&f, "/dashboard/runs/r-review", Some(&me)).await;
    assert_eq!(run.status, StatusCode::OK);
    assert!(run.body.contains("<h2>Lanes</h2>") && run.body.contains("model-x"));
    assert!(run.body.contains("<h2>Findings</h2>") && run.body.contains("<code>src/a.rs:4</code>"));
    assert!(
        run.body.contains("<a href=\"/dashboard/events/e-1\">"),
        "links stay in the dashboard"
    );
    assert!(run.body.contains("<nav>"));

    let events = get(&f, "/dashboard/events?source=github_webhook", Some(&me)).await;
    assert!(
        events.body.contains("<b>review</b>: started"),
        "{}",
        events.body
    );
    assert!(
        events
            .body
            .contains("<a href=\"/dashboard/runs/r-review\">r-review</a>")
    );
    let event = get(&f, "/dashboard/events/e-1", Some(&me)).await;
    assert!(
        event.body.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "the payload is escaped"
    );
    assert!(!event.body.contains("<img"));
    let none = get(&f, "/dashboard/events?source=api", Some(&me)).await;
    assert!(none.body.contains("None."));

    let health = get(&f, "/dashboard/health", Some(&me)).await;
    assert_eq!(health.status, StatusCode::OK);
    assert!(
        health.body.contains("<td>database</td><td>ok</td>"),
        "{}",
        health.body
    );
    assert!(health.body.contains("review, plan"));

    let running = get(&f, "/dashboard/running.json", Some(&me)).await;
    let running: Value = serde_json::from_str(&running.body).unwrap();
    assert_eq!(running["total"], 1);
    let rows = &running["runs"];
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], "r-review");
    assert_eq!(
        rows[0]["about_url"],
        "https://github.com/docspec/app/pull/7"
    );
    let script = get(&f, "/dashboard/app.js", Some(&me)).await;
    assert!(script.body.contains("textContent"));
    assert!(!script.body.contains("innerHTML"));
}
