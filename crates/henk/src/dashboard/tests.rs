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
        live_runs: crate::liveness::LiveRuns::default(),
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
    let dashboard =
        Arc::new(Dashboard::new(app, coordinator, bus, dashboard_config, &secrets).unwrap());
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
async fn a_sessions_transcript_reads_behind_sign_in_and_escaped() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let store = &f.dashboard.app.store;
    let review = RunId::parse("r-review").unwrap();
    store
        .start_lane(&review, "check lane/a?", "model-x")
        .await
        .unwrap();
    let long: Vec<String> = (0..40).map(|n| format!("line {n}")).collect();
    let body = json!({
        "session": "lane-a", "model": "model-x", "stop": "EndTurn", "turns": 1,
        "usage": {"input_tokens": 7, "output_tokens": 2},
        "system": "You review <b>this</b>.",
        "messages": [
            {"role": "user", "blocks": [{"text": "<script>alert(1)</script>"}]},
            {"role": "assistant", "blocks": [{"tool_call": {"id": "c", "name": "read_file", "arguments": {"parsed": {"path": "a.rs"}}}}]},
            {"role": "user", "blocks": [{"tool_result": {"call_id": "c", "content": long.join("\n"), "is_error": false}}]}
        ]
    })
    .to_string();
    for session in ["lane-a", "check lane/a?"] {
        store
            .record_transcript(
                &review,
                &henk_store::TranscriptRecord {
                    at: String::new(),
                    session: session.into(),
                    model: "model-x".into(),
                    stop: "EndTurn".into(),
                    turns: 1,
                    bytes: body.len() as u64,
                    body: body.clone(),
                },
            )
            .await
            .unwrap();
    }

    let unsigned = get(&f, "/dashboard/runs/r-review/transcripts/lane-a", None).await;
    assert_eq!(unsigned.status, StatusCode::SEE_OTHER);
    assert!(!unsigned.body.contains("alert"));

    let me = signed_in(&f, ALLOWED);
    let run = get(&f, "/dashboard/runs/r-review", Some(&me)).await;
    assert!(
        run.body
            .contains("<a href=\"/dashboard/runs/r-review/transcripts/lane-a\""),
        "{}",
        run.body
    );
    assert!(
        run.body
            .contains("<a href=\"/dashboard/runs/r-review/transcripts/check%20lane%2Fa%3F\""),
        "a session name stays in its path segment: {}",
        run.body
    );

    let page = get(&f, "/dashboard/runs/r-review/transcripts/lane-a", Some(&me)).await;
    assert_eq!(page.status, StatusCode::OK);
    for expected in [
        "<h1>Transcript of lane-a</h1>",
        "You review &lt;b&gt;this&lt;/b&gt;.",
        "&lt;script&gt;alert(1)&lt;/script&gt;",
        "Call <code>read_file</code>",
        "<details><summary>Result, 40 lines</summary>",
        "<a href=\"/dashboard/runs/r-review\">r-review</a>",
    ] {
        assert!(page.body.contains(expected), "{expected} in {}", page.body);
    }
    assert!(!page.body.contains("<script>alert"));
    let escaped = get(
        &f,
        "/dashboard/runs/r-review/transcripts/check%20lane%2Fa%3F",
        Some(&me),
    )
    .await;
    assert_eq!(escaped.status, StatusCode::OK);
    assert_eq!(
        get(&f, "/dashboard/runs/r-review/transcripts/lane-b", Some(&me))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, "/dashboard/runs/r-plan/transcripts/lane-a", Some(&me))
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a transcript belongs to its own run"
    );
}

#[tokio::test]
async fn run_detail_events_health_and_the_poller_answer() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let me = signed_in(&f, ALLOWED);
    let review = RunId::parse("r-review").unwrap();
    f.dashboard
        .app
        .store
        .record_draft(
            &review,
            &henk_store::DraftRecord {
                at: String::new(),
                draft: "d1".into(),
                lane: "lane-a".into(),
                model: "model-x".into(),
                kind: "finding".into(),
                path: "src/a.rs".into(),
                line: 4,
                target: String::new(),
                body: "<b>x</b> is never set.".into(),
                decision: None,
            },
        )
        .await
        .unwrap();
    f.dashboard
        .app
        .store
        .decide_draft(
            &review,
            "d1",
            &henk_store::DraftDecision {
                at: String::new(),
                verdict: henk_store::DraftVerdict::SameAs,
                checker: "model-y".into(),
                reason: "Same as c-77.".into(),
                same_as: "c-77".into(),
                comment_id: "c-77".into(),
            },
        )
        .await
        .unwrap();

    let run = get(&f, "/dashboard/runs/r-review", Some(&me)).await;
    assert_eq!(run.status, StatusCode::OK);
    assert!(
        run.body.contains("<h2>Drafts</h2>")
            && run.body.contains("<td>d1</td><td>lane-a</td><td><code>src/a.rs:4</code></td><td>&lt;b&gt;x&lt;/b&gt; is never set.</td><td>same as c-77</td><td>model-y</td><td>c-77</td><td>Same as c-77.</td>"),
        "{}",
        run.body
    );
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
async fn a_start_from_the_dashboard_is_an_event_like_the_api_with_who_asked() {
    let f = fixture("https://127.0.0.1:9");
    let session = Session::fresh(ALLOWED, "alice".to_owned()).unwrap();
    let (cookie, csrf) = signed_in_as(&f, &session);
    let origin = [("origin", ORIGIN)];

    let started = post(
        &f,
        "/dashboard/start",
        &cookie,
        &origin,
        &format!("csrf={csrf}&kind=plan&url=https%3A%2F%2Fgithub.com%2Fdocspec%2Fapp%2Fissues%2F3&commit=&note=split+it"),
    )
    .await;
    assert_eq!(started.status, StatusCode::SEE_OTHER, "{}", started.body);
    let to = started.headers[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let event = EventId::parse(to.strip_prefix("/dashboard/events/").unwrap()).unwrap();
    // Published like an API request: recorded and handled in the background.
    let outcomes = outcomes_of(&f, &event, 3).await;
    let recorded = f
        .dashboard
        .app
        .store
        .inbound_event(&event)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorded.source, "dashboard");
    assert_eq!(recorded.kind, "plan_requested");
    assert_eq!(recorded.requester.as_deref(), Some("github:1234"));
    let plan = outcomes.into_iter().find(|o| o.listener == "plan").unwrap();
    assert_eq!(plan.outcome, "started", "{}", plan.detail);
    let page = get(&f, &to, Some(&cookie)).await;
    assert!(page.body.contains("Asked by: github:1234"), "{}", page.body);

    let elsewhere = post(
        &f,
        "/dashboard/start",
        &cookie,
        &origin,
        &format!("csrf={csrf}&kind=plan&url=https%3A%2F%2Fgithub.com%2Fother%2Fapp%2Fissues%2F3"),
    )
    .await;
    assert_eq!(elsewhere.status, StatusCode::SEE_OTHER);
    let to = elsewhere.headers[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let event = EventId::parse(to.strip_prefix("/dashboard/events/").unwrap()).unwrap();
    let refused = outcomes_of(&f, &event, 3)
        .await
        .into_iter()
        .find(|o| o.listener == "plan")
        .unwrap();
    assert_eq!(refused.outcome, "ignored");
    assert!(
        refused.detail.contains("not on the allowlist"),
        "{}",
        refused.detail
    );

    for form in [
        format!("csrf={csrf}&kind=plan&url=not-a-url"),
        format!(
            "csrf={csrf}&kind=review&url=https%3A%2F%2Fgithub.com%2Fdocspec%2Fapp%2Fpull%2F7&commit=xyz"
        ),
        format!(
            "csrf={csrf}&kind=plan&url=https%3A%2F%2Fgithub.com%2Fdocspec%2Fapp%2Fissues%2F3&commit=0123456789abcdef0123456789abcdef01234567"
        ),
        format!(
            "csrf={csrf}&kind=deploy&url=https%3A%2F%2Fgithub.com%2Fdocspec%2Fapp%2Fissues%2F3"
        ),
    ] {
        let answer = post(&f, "/dashboard/start", &cookie, &origin, &form).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{form}");
    }
}

#[tokio::test]
async fn an_action_without_its_token_or_from_elsewhere_is_refused_and_does_nothing() {
    let f = fixture("https://127.0.0.1:9");
    let session = Session::fresh(ALLOWED, "alice".to_owned()).unwrap();
    let (cookie, csrf) = signed_in_as(&f, &session);
    let (_, other_csrf) = signed_in_as(&f, &Session::fresh(ALLOWED, "alice".to_owned()).unwrap());
    let form = |token: &str| {
        format!("csrf={token}&kind=plan&url=https%3A%2F%2Fgithub.com%2Fdocspec%2Fapp%2Fissues%2F3")
    };
    let no_token = "kind=plan&url=https%3A%2F%2Fgithub.com%2Fdocspec%2Fapp%2Fissues%2F3".to_owned();
    let cases: Vec<Case> = vec![
        ("no token", vec![("origin", ORIGIN)], no_token),
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
        let answer = post(&f, "/dashboard/start", &cookie, &headers, &body).await;
        assert_eq!(
            answer.status,
            StatusCode::FORBIDDEN,
            "{what}: {}",
            answer.body
        );
    }
    let removed = signed_in(&f, 999);
    let answer = post(
        &f,
        "/dashboard/start",
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
    let none = f
        .dashboard
        .app
        .store
        .list_inbound_events(&henk_store::EventFilter::default(), Page::new(10, 0))
        .await
        .unwrap();
    assert!(none.is_empty(), "nothing refused was published");

    let by_referer = post(
        &f,
        "/dashboard/start",
        &cookie,
        &[("referer", "https://henk.example/dashboard")],
        &form(&csrf),
    )
    .await;
    assert_eq!(
        by_referer.status,
        StatusCode::SEE_OTHER,
        "the dashboard's own referer"
    );

    let logout = post(&f, "/dashboard/logout", &cookie, &[("origin", ORIGIN)], "").await;
    assert_eq!(
        logout.status,
        StatusCode::FORBIDDEN,
        "signing out needs the token too"
    );
    let logout = post(
        &f,
        "/dashboard/logout",
        &cookie,
        &[("origin", ORIGIN)],
        &format!("csrf={csrf}"),
    )
    .await;
    assert_eq!(logout.status, StatusCode::OK);
}

#[tokio::test]
async fn cancelling_fires_the_runs_token_and_records_who_asked() {
    let f = fixture("https://127.0.0.1:9");
    let store = Arc::clone(&f.dashboard.app.store);
    seed(&f).await;
    let session = Session::fresh(ALLOWED, "alice".to_owned()).unwrap();
    let (cookie, csrf) = signed_in_as(&f, &session);
    let origin = [("origin", ORIGIN)];
    let running = RunId::parse("r-review").unwrap();
    assert_eq!(
        store.run(&running).await.unwrap().unwrap().status,
        RunStatus::Running
    );

    let page = get(&f, "/dashboard/runs/r-review", Some(&cookie)).await;
    assert!(
        page.body
            .contains("action=\"/dashboard/runs/r-review/cancel\""),
        "{}",
        page.body
    );
    assert!(page.body.contains(&csrf));
    let finished = get(&f, "/dashboard/runs/r-plan", Some(&cookie)).await;
    assert!(
        !finished.body.contains("Cancel this run"),
        "only a running run"
    );
    let finished = get(&f, "/dashboard/runs/r-plan", Some(&cookie)).await;
    assert!(
        !finished.body.contains("Cancel this run"),
        "only a running run"
    );

    let not_here = post(
        &f,
        "/dashboard/runs/r-review/cancel",
        &cookie,
        &origin,
        &format!("csrf={csrf}"),
    )
    .await;
    assert_eq!(
        not_here.status,
        StatusCode::CONFLICT,
        "nothing here runs it"
    );

    let token = tokio_util::sync::CancellationToken::new();
    let _cancellable = f
        .dashboard
        .app
        .cancels
        .register(running.clone(), token.clone());
    let cancelled = post(
        &f,
        "/dashboard/runs/r-review/cancel",
        &cookie,
        &origin,
        &format!("csrf={csrf}"),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::SEE_OTHER);
    assert_eq!(
        cancelled.headers[header::LOCATION],
        "/dashboard/runs/r-review"
    );
    assert!(token.is_cancelled());
    assert_eq!(
        f.dashboard.app.cancels.cancelled_by(&running).as_deref(),
        Some("github:1234")
    );
    let requests: Vec<_> = store
        .inbound_events_for_run(&running)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "cancel_requested")
        .collect();
    assert_eq!(requests.len(), 2, "both requests are recorded");
    assert!(
        requests
            .iter()
            .all(|e| e.source == "dashboard" && e.requester.as_deref() == Some("github:1234"))
    );

    let forged = post(
        &f,
        "/dashboard/runs/r-review/cancel",
        &cookie,
        &origin,
        "csrf=AAAA",
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
}
