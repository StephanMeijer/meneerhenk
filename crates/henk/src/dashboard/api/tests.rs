#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use henk_store::{
    DraftDecision, DraftRecord, DraftVerdict, InboundEvent, LaneStatus, NewRun, RunStatus,
    ToolCallRecord, TranscriptRecord,
};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::types;
use crate::dashboard::auth::CSRF_HEADER;
use crate::dashboard::session::Session;
use crate::dashboard::tests::{
    ALLOWED, Fixture, fixture, fixture_with, outcomes_of, seed, signed_in_as,
};

const ORIGIN: &str = "https://henk.example";

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap().to_owned()
    }
}

async fn call(
    f: &Fixture,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<(&str, &str)>,
) -> Answer {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some((content_type, text)) => {
            request = request.header(header::CONTENT_TYPE, content_type);
            Body::from(text.to_owned())
        }
        None => Body::empty(),
    };
    let response = f
        .router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get(f: &Fixture, uri: &str, cookie: &str) -> Answer {
    call(f, Method::GET, uri, &[("cookie", cookie)], None).await
}

/// A signed-in viewer's API action: their cookie, token and origin.
async fn act(f: &Fixture, uri: &str, cookie: &str, csrf: &str, body: &Value) -> Answer {
    call(
        f,
        Method::POST,
        uri,
        &[("cookie", cookie), (CSRF_HEADER, csrf), ("origin", ORIGIN)],
        Some(("application/json", &body.to_string())),
    )
    .await
}

fn viewer(f: &Fixture) -> (String, String) {
    signed_in_as(f, &Session::fresh(ALLOWED, "alice".to_owned()).unwrap())
}

const ROUTES: &[(&str, &str)] = &[
    ("GET", "/dashboard/api/v1/me"),
    ("GET", "/dashboard/api/v1/health"),
    ("GET", "/dashboard/api/v1/runs"),
    ("GET", "/dashboard/api/v1/runs/count"),
    ("GET", "/dashboard/api/v1/quality"),
    ("GET", "/dashboard/api/v1/drafts"),
    ("GET", "/dashboard/api/v1/tool-calls/summary"),
    ("GET", "/dashboard/api/v1/tool-calls"),
    ("GET", "/dashboard/api/v1/runs/r-review"),
    ("GET", "/dashboard/api/v1/runs/r-review/events"),
    ("GET", "/dashboard/api/v1/runs/r-review/tool-calls"),
    ("GET", "/dashboard/api/v1/runs/r-review/transcripts/lane-a"),
    ("GET", "/dashboard/api/v1/events"),
    ("GET", "/dashboard/api/v1/events/e-1"),
    ("POST", "/dashboard/api/v1/runs"),
    ("POST", "/dashboard/api/v1/runs/r-review/cancel"),
];

#[tokio::test]
async fn no_route_answers_without_a_session_or_to_an_id_off_the_list_and_none_redirects() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (outsider, _) = signed_in_as(&f, &Session::fresh(999, "mallory".to_owned()).unwrap());
    for (method, uri) in ROUTES {
        let method: Method = method.parse().unwrap();
        let body = (method == Method::POST).then_some(("application/json", "{}"));
        let anonymous = call(&f, method.clone(), uri, &[("origin", ORIGIN)], body).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(anonymous.code(), "unauthenticated", "{uri}");
        assert!(anonymous.headers.get(header::LOCATION).is_none(), "{uri}");

        let off_the_list = call(
            &f,
            method,
            uri,
            &[("cookie", outsider.as_str()), ("origin", ORIGIN)],
            body,
        )
        .await;
        assert_eq!(off_the_list.status, StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(off_the_list.code(), "forbidden", "{uri}");
        assert!(
            off_the_list.body.contains("github:999")
                && off_the_list
                    .body
                    .contains("Ask an operator to add this id."),
            "the refusal names the id to add (#232): {}",
            off_the_list.body
        );
        assert!(
            !off_the_list.body.contains("mallory"),
            "the id, never the login"
        );
    }
}

/// Why a request is refused, its headers and its content type.
type Refusal<'a> = (&'a str, Vec<(&'a str, &'a str)>, &'a str);

#[tokio::test]
async fn an_action_needs_its_token_the_dashboards_origin_and_a_json_body() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, csrf) = viewer(&f);
    let start = json!({"kind": "review", "url": "https://github.com/docspec/app/pull/7"});
    let body = start.to_string();
    let other = signed_in_as(&f, &Session::fresh(ALLOWED, "alice".to_owned()).unwrap()).1;
    let refusals: &[Refusal<'_>] = &[
        (
            "csrf: no token",
            vec![("cookie", cookie.as_str()), ("origin", ORIGIN)],
            "application/json",
        ),
        (
            "csrf: another session's token",
            vec![
                ("cookie", cookie.as_str()),
                (CSRF_HEADER, other.as_str()),
                ("origin", ORIGIN),
            ],
            "application/json",
        ),
        (
            "another origin",
            vec![
                ("cookie", cookie.as_str()),
                (CSRF_HEADER, csrf.as_str()),
                ("origin", "https://evil.example"),
            ],
            "application/json",
        ),
        (
            "no origin at all",
            vec![("cookie", cookie.as_str()), (CSRF_HEADER, csrf.as_str())],
            "application/json",
        ),
    ];
    for (why, headers, content_type) in refusals {
        let answer = call(
            &f,
            Method::POST,
            "/dashboard/api/v1/runs",
            headers,
            Some((content_type, &body)),
        )
        .await;
        assert_eq!(
            answer.status,
            StatusCode::FORBIDDEN,
            "{why}: {}",
            answer.body
        );
        // A missing or foreign token says so, for the client to offer a
        // reload; the other refusals are plain refusals.
        let code = if why.starts_with("csrf:") {
            "csrf"
        } else {
            "forbidden"
        };
        assert_eq!(answer.code(), code, "{why}");
    }
    let form = call(
        &f,
        Method::POST,
        "/dashboard/api/v1/runs",
        &[
            ("cookie", cookie.as_str()),
            (CSRF_HEADER, csrf.as_str()),
            ("origin", ORIGIN),
        ],
        Some((
            "application/x-www-form-urlencoded",
            "kind=review&url=https://github.com/docspec/app/pull/7",
        )),
    )
    .await;
    assert_eq!(
        form.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "a form is not JSON"
    );
    assert_eq!(form.code(), "unsupported_media_type");
    let events = f
        .dashboard
        .app
        .store
        .list_inbound_events(
            &henk_store::EventFilter::default(),
            henk_store::Page::new(50, 0),
        )
        .await
        .unwrap();
    assert!(events.is_empty(), "nothing refused was published");
}

#[tokio::test]
async fn me_says_who_is_signed_in_with_their_token_and_nothing_is_cached() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, csrf) = viewer(&f);
    let me = get(&f, "/dashboard/api/v1/me", &cookie).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(
        me.json(),
        json!({"github_id": ALLOWED, "login": "alice", "csrf": csrf, "startable": ["review", "plan"]})
    );
    assert_eq!(me.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(me.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(
        me.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );

    let unknown = get(&f, "/dashboard/api/v1/nothing/here", &cookie).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.code(), "not_found");
}

/// A draft, a tool call and a transcript on `r-review`, besides `seed`.
async fn seed_review_record(f: &Fixture) {
    let store = &f.dashboard.app.store;
    let review = RunId::parse("r-review").unwrap();
    store
        .record_draft(
            &review,
            &DraftRecord {
                at: String::new(),
                draft: "d1".into(),
                lane: "lane-a".into(),
                model: "model-x".into(),
                kind: "finding".into(),
                path: "src/a.rs".into(),
                line: 4,
                target: String::new(),
                body: "The <b>loop</b> never ends.".into(),
                decision: None,
            },
        )
        .await
        .unwrap();
    store
        .decide_draft(
            &review,
            "d1",
            &DraftDecision {
                at: String::new(),
                verdict: DraftVerdict::Confirmed,
                checker: "model-y".into(),
                reason: "src/a.rs:4 loops forever.".into(),
                same_as: String::new(),
                comment_id: "c-77".into(),
            },
        )
        .await
        .unwrap();
    for (turn, tool, outcome) in [
        (1, "get_file_diff", "ok"),
        (2, "read_file", "refused_scope"),
        (3, "read_file", "ok"),
    ] {
        store
            .record_tool_call(
                &review,
                &ToolCallRecord {
                    at: String::new(),
                    session: "lane-a".into(),
                    model: "model-x".into(),
                    turn,
                    tool: tool.into(),
                    origin: "henk".into(),
                    outcome: outcome.into(),
                    arguments: "{\"path\":\"src/a.rs\"}".into(),
                    arguments_len: 18,
                    result_chars: 120,
                    elapsed_ms: 5,
                },
            )
            .await
            .unwrap();
    }
    let body = json!({
        "session": "lane-a",
        "model": "model-x",
        "stop": "EndTurn",
        "turns": 1,
        "usage": {"input_tokens": 10, "cache_read_tokens": 5, "output_tokens": 3},
        "system": "You review.",
        "messages": [
            {"role": "user", "blocks": [{"text": "Review <this>."}]},
            {"role": "assistant", "blocks": [
                {"text": "Looking."},
                {"tool_call": {"id": "c1", "name": "read_file", "arguments": {"parsed": {"path": "src/a.rs"}}}}
            ]},
            {"role": "user", "blocks": [{"tool_result": {"id": "c1", "content": "fn main() {}", "is_error": false}}]}
        ]
    })
    .to_string();
    store
        .record_transcript(
            &review,
            &TranscriptRecord {
                at: String::new(),
                session: "lane-a".into(),
                model: "model-x".into(),
                stop: "EndTurn".into(),
                turns: 1,
                bytes: u64::try_from(body.len()).unwrap(),
                body,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_superseded_run_says_so_and_names_its_replacement_and_lanes_say_how_they_ended() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&f);
    let store = &f.dashboard.app.store;
    for id in ["r-old", "r-new"] {
        store
            .create_run(&NewRun {
                id: RunId::parse(id).unwrap(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "docspec/app".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "new commits".into(),
                link: format!("{ORIGIN}/runs/{id}"),
            })
            .await
            .unwrap();
    }
    let (old, new) = (
        RunId::parse("r-old").unwrap(),
        RunId::parse("r-new").unwrap(),
    );
    for lane in ["lane-a", "lane-b"] {
        store.start_lane(&old, lane, "m").await.unwrap();
    }
    store
        .finish_lane(&old, "lane-a", LaneStatus::TimedOut, 30, 1, 1, None)
        .await
        .unwrap();
    store.drop_running_lanes(&old, "cancelled").await.unwrap();
    store
        .supersede_run(&old, &new, "superseded by a review of a newer commit")
        .await
        .unwrap();

    let listed = get(&f, "/dashboard/api/v1/runs?status=superseded", &cookie).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    let listed = listed.json();
    assert_eq!(ids(&listed), ["r-old"]);
    assert_eq!(listed["items"][0]["status"], "superseded");
    assert_eq!(listed["items"][0]["superseded_by"], "r-new");

    store
        .stage(
            &old,
            &henk_store::StageWrite::now(
                henk_store::Stage::Diff,
                henk_store::StageState::Done,
                "3 files, +4 -1; 0 not reviewed",
            ),
        )
        .await
        .unwrap();
    let detail = get(&f, "/dashboard/api/v1/runs/r-old", &cookie)
        .await
        .json();
    assert_eq!(detail["run"]["superseded_by"], "r-new");
    assert_eq!(detail["stages"][0]["name"], "diff");
    assert_eq!(detail["stages"][0]["state"], "done");
    assert_eq!(
        detail["stages"][0]["detail"],
        "3 files, +4 -1; 0 not reviewed"
    );
    assert!(detail["stages"][0]["ended_at"].is_string());
    assert!(detail["lanes"][0]["started_at"].is_string());
    assert!(detail["lanes"][0]["finished_at"].is_string());
    assert_eq!(detail["lanes"][0]["status"], "timed_out");
    assert_eq!(detail["lanes"][0]["error"], Value::Null);
    assert_eq!(detail["lanes"][1]["status"], "did_not_finish");
    assert_eq!(detail["lanes"][1]["error"], "cancelled");
    let replacing = get(&f, "/dashboard/api/v1/runs/r-new", &cookie)
        .await
        .json();
    assert_eq!(replacing["run"]["superseded_by"], Value::Null);
}

#[tokio::test]
async fn runs_list_and_one_run_in_full_with_text_as_stored() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_review_record(&f).await;
    let (cookie, _) = viewer(&f);

    let runs = get(&f, "/dashboard/api/v1/runs", &cookie).await;
    assert_eq!(runs.status, StatusCode::OK, "{}", runs.body);
    let runs = runs.json();
    let items = runs["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "r-plan", "newest first");
    assert_eq!(items[0]["kind"], "plan");
    assert_eq!(items[0]["status"], "finished");
    assert_eq!(
        items[0]["trigger"], "<script>alert(1)</script>",
        "text comes back as stored, for the client to show as text"
    );
    assert_eq!(items[1]["status"], "running");
    assert_eq!(items[1]["platform"], "github");
    assert!(
        items[1]["target_url"]
            .as_str()
            .unwrap()
            .ends_with("/docspec/app/pull/7")
    );
    assert_eq!(runs["next"], Value::Null, "one page holds them");

    let detail = get(&f, "/dashboard/api/v1/runs/r-review", &cookie).await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    let detail = detail.json();
    assert_eq!(detail["run"]["id"], "r-review");
    assert_eq!(detail["lanes"][0]["name"], "lane-a");
    assert_eq!(detail["lanes"][0]["turns"], 3);
    assert_eq!(detail["lanes"][0]["status"], "finished");
    assert_eq!(detail["findings"][0]["comment_id"], "c-77");
    assert_eq!(detail["findings"][0]["action"], "posted");
    assert_eq!(detail["drafts"][0]["id"], "d1");
    assert_eq!(detail["drafts"][0]["body"], "The <b>loop</b> never ends.");
    assert_eq!(detail["drafts"][0]["decision"]["verdict"], "confirmed");
    assert_eq!(detail["drafts"][0]["decision"]["checker"], "model-y");
    assert_eq!(detail["transcripts"][0]["session"], "lane-a");
    let reads = detail["tool_usage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["tool"] == "read_file")
        .unwrap();
    assert_eq!(reads["calls"], 2);
    assert_eq!(reads["refusals"], 1);
    assert_eq!(detail["requests"][0]["id"], "e-1");

    let missing = get(&f, "/dashboard/api/v1/runs/r-none", &cookie).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "not_found");
    let bad = get(&f, "/dashboard/api/v1/runs/bad%20id", &cookie).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad.code(), "bad_request");
}

#[tokio::test]
async fn a_query_that_does_not_parse_is_a_json_bad_request() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_review_record(&f).await;
    let (cookie, _) = viewer(&f);
    for uri in [
        "/dashboard/api/v1/runs?target=abc",
        "/dashboard/api/v1/runs?limit=-1",
        "/dashboard/api/v1/runs?limit=x",
        "/dashboard/api/v1/runs/r-review/events?limit=x",
        "/dashboard/api/v1/events?limit=x",
    ] {
        let bad = get(&f, uri, &cookie).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(bad.code(), "bad_request", "{uri}");
        assert!(
            bad.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json"),
            "{uri}"
        );
        assert!(
            henk_domain::text::style_violations(bad.json()["error"]["message"].as_str().unwrap())
                .is_empty(),
            "{uri}"
        );
    }
    let unsigned = call(
        &f,
        Method::GET,
        "/dashboard/api/v1/runs?target=abc",
        &[],
        None,
    )
    .await;
    assert_eq!(unsigned.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_runs_tool_calls_timeline_and_transcript_read_by_filter() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_review_record(&f).await;
    let store = &f.dashboard.app.store;
    let review = RunId::parse("r-review").unwrap();
    for n in 0..3 {
        store
            .event(&review, "info", &format!("line {n}"))
            .await
            .unwrap();
    }
    let (cookie, _) = viewer(&f);

    let calls = get(&f, "/dashboard/api/v1/runs/r-review/tool-calls", &cookie)
        .await
        .json();
    assert_eq!(calls.as_array().unwrap().len(), 3);
    let refused = get(
        &f,
        "/dashboard/api/v1/runs/r-review/tool-calls?outcome=refused_scope",
        &cookie,
    )
    .await
    .json();
    assert_eq!(refused.as_array().unwrap().len(), 1);
    assert_eq!(refused[0]["turn"], 2);
    assert_eq!(refused[0]["arguments"], "{\"path\":\"src/a.rs\"}");
    let reads = get(
        &f,
        "/dashboard/api/v1/runs/r-review/tool-calls?session=lane-a&tool=read_file",
        &cookie,
    )
    .await
    .json();
    assert_eq!(reads.as_array().unwrap().len(), 2);

    let first = get(
        &f,
        "/dashboard/api/v1/runs/r-review/events?limit=2",
        &cookie,
    )
    .await
    .json();
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let next = first["next"].as_str().unwrap();
    let rest = get(
        &f,
        &format!("/dashboard/api/v1/runs/r-review/events?limit=2&cursor={next}"),
        &cookie,
    )
    .await
    .json();
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["items"][0]["message"], "line 2");
    assert_eq!(rest["next"], Value::Null);
    let bad = get(
        &f,
        "/dashboard/api/v1/runs/r-review/events?cursor=x",
        &cookie,
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let transcript = get(
        &f,
        "/dashboard/api/v1/runs/r-review/transcripts/lane-a",
        &cookie,
    )
    .await;
    assert_eq!(transcript.status, StatusCode::OK, "{}", transcript.body);
    let transcript = transcript.json();
    assert_eq!(transcript["system"], "You review.");
    assert_eq!(transcript["prompt_tokens"], 15);
    assert_eq!(
        transcript["messages"][0]["parts"][0],
        json!({"type": "text", "text": "Review <this>."})
    );
    assert_eq!(
        transcript["messages"][1]["parts"][1],
        json!({"type": "call", "name": "read_file", "arguments": "{\"path\":\"src/a.rs\"}"})
    );
    assert_eq!(transcript["messages"][2]["turn"], 1);
    assert_eq!(
        transcript["messages"][2]["parts"][0],
        json!({"type": "result", "error": false, "content": "fn main() {}"})
    );
    let none = get(
        &f,
        "/dashboard/api/v1/runs/r-review/transcripts/lane-z",
        &cookie,
    )
    .await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);
}

/// `n` review runs on targets 1..=n of `docspec/app`, oldest first.
async fn many_runs(f: &Fixture, n: u64) {
    for i in 1..=n {
        f.dashboard
            .app
            .store
            .create_run(&NewRun {
                id: RunId::parse(format!("r-{i:03}")).unwrap(),
                kind: if i % 10 == 0 {
                    RunKind::Plan
                } else {
                    RunKind::Review
                },
                platform: Platform::GitHub,
                repo: "docspec/app".into(),
                target: i,
                commit: None,
                requester: None,
                trigger: "opened".into(),
                link: String::new(),
            })
            .await
            .unwrap();
    }
}

fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn runs_page_by_cursor_without_overlap_or_gaps_and_filter() {
    let f = fixture("https://127.0.0.1:9");
    many_runs(&f, 120).await;
    let (cookie, _) = viewer(&f);

    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    let mut uri = "/dashboard/api/v1/runs".to_owned();
    loop {
        let page = get(&f, &uri, &cookie).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        let page = page.json();
        sizes.push(page["items"].as_array().unwrap().len());
        seen.extend(ids(&page));
        // A run started meanwhile does not shift the next page.
        if seen.len() == 50 {
            many_runs_after(&f).await;
        }
        match page["next"].as_str() {
            Some(next) => uri = format!("/dashboard/api/v1/runs?cursor={next}"),
            None => break,
        }
    }
    assert_eq!(sizes, [50, 50, 20]);
    let expected: Vec<String> = (1..=120).rev().map(|i| format!("r-{i:03}")).collect();
    assert_eq!(seen, expected, "every run once, newest first");

    let plans = get(&f, "/dashboard/api/v1/runs?kind=plan&limit=5", &cookie)
        .await
        .json();
    assert_eq!(ids(&plans), ["r-120", "r-110", "r-100", "r-090", "r-080"]);
    let next = plans["next"].as_str().unwrap();
    let more = get(
        &f,
        &format!("/dashboard/api/v1/runs?kind=plan&limit=5&cursor={next}"),
        &cookie,
    )
    .await
    .json();
    assert_eq!(ids(&more), ["r-070", "r-060", "r-050", "r-040", "r-030"]);
    let one = get(&f, "/dashboard/api/v1/runs?target=42", &cookie)
        .await
        .json();
    assert_eq!(ids(&one), ["r-042"]);
    let running = get(
        &f,
        "/dashboard/api/v1/runs?status=running&repo=docspec/app&platform=github&limit=1",
        &cookie,
    )
    .await
    .json();
    assert_eq!(ids(&running), ["r-999"]);
    let elsewhere = get(&f, "/dashboard/api/v1/runs?repo=docspec/other", &cookie)
        .await
        .json();
    assert!(ids(&elsewhere).is_empty());

    let all = f
        .dashboard
        .app
        .store
        .list_runs(
            &henk_store::RunFilter::default(),
            henk_store::Page::new(100, 0),
        )
        .await
        .unwrap();
    let at = |id: &str| {
        all.iter()
            .find(|r| r.id.as_str() == id)
            .unwrap()
            .started_at
            .clone()
    };
    let window = get(
        &f,
        &format!(
            "/dashboard/api/v1/runs?since={}&until={}",
            urlencode(&at("r-997")),
            urlencode(&at("r-999"))
        ),
        &cookie,
    )
    .await;
    assert_eq!(window.status, StatusCode::OK, "{}", window.body);
    assert_eq!(ids(&window.json()), ["r-998", "r-997"]);

    for (query, why) in [
        ("cursor=bm90IGEgY3Vyc29y", "a cursor not given out"),
        ("cursor=%%%", "not base64"),
        ("kind=lunch", "an unknown kind"),
        ("since=yesterday", "not a time"),
        ("target=-1", "not a number"),
    ] {
        let answer = get(&f, &format!("/dashboard/api/v1/runs?{query}"), &cookie).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{why}: {}",
            answer.body
        );
    }
}

/// Three runs started after the first 120, so they sort first.
async fn many_runs_after(f: &Fixture) {
    for i in 997..=999 {
        tokio::time::sleep(Duration::from_millis(2)).await;
        f.dashboard
            .app
            .store
            .create_run(&NewRun {
                id: RunId::parse(format!("r-{i}")).unwrap(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "docspec/app".into(),
                target: i,
                commit: None,
                requester: None,
                trigger: "opened".into(),
                link: String::new(),
            })
            .await
            .unwrap();
    }
}

fn urlencode(text: &str) -> String {
    text.replace(':', "%3A").replace('+', "%2B")
}

#[tokio::test]
async fn inbound_events_list_by_cursor_and_one_reads_with_its_payload() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let store = &f.dashboard.app.store;
    for (id, at) in [
        ("e-2", "2026-10-06T00:00:01Z"),
        ("e-3", "2026-10-06T00:00:01Z"),
    ] {
        store
            .record_event(&InboundEvent {
                id: EventId::parse(id).unwrap(),
                received_at: at.into(),
                source: "api".into(),
                kind: "review_requested".into(),
                repo: Some("docspec/app".into()),
                target: Some(7),
                payload: None,
                requester: Some("github:1234".into()),
            })
            .await
            .unwrap();
    }
    let (cookie, _) = viewer(&f);

    let first = get(&f, "/dashboard/api/v1/events?limit=2", &cookie)
        .await
        .json();
    let first_ids: Vec<_> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(first_ids, ["e-3", "e-2"], "a tie on the time goes by id");
    let next = first["next"].as_str().unwrap();
    let rest = get(
        &f,
        &format!("/dashboard/api/v1/events?limit=2&cursor={next}"),
        &cookie,
    )
    .await
    .json();
    assert_eq!(rest["items"][0]["event"]["id"], "e-1");
    assert_eq!(rest["items"][0]["outcomes"][0]["listener"], "review");
    assert_eq!(rest["items"][0]["outcomes"][0]["run_id"], "r-review");
    assert_eq!(rest["next"], Value::Null);
    let api = get(&f, "/dashboard/api/v1/events?source=api", &cookie)
        .await
        .json();
    assert_eq!(api["items"].as_array().unwrap().len(), 2);

    let one = get(&f, "/dashboard/api/v1/events/e-1", &cookie).await;
    assert_eq!(one.status, StatusCode::OK);
    let one = one.json();
    assert_eq!(
        one["payload"], "{\"title\": \"<img src=x onerror=alert(1)>\"}",
        "the payload as received, as text"
    );
    assert_eq!(one["outcomes"][0]["outcome"], "started");
    let missing = get(&f, "/dashboard/api/v1/events/e-none", &cookie).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_start_through_the_api_is_an_event_with_who_asked() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, csrf) = viewer(&f);

    let started = act(
        &f,
        "/dashboard/api/v1/runs",
        &cookie,
        &csrf,
        &json!({"kind": "plan", "url": "https://github.com/docspec/app/issues/12", "note": "small steps"}),
    )
    .await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.body);
    let id = EventId::parse(started.json()["event_id"].as_str().unwrap()).unwrap();
    assert!(!outcomes_of(&f, &id, 1).await.is_empty());
    let event = f
        .dashboard
        .app
        .store
        .inbound_event(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.source, "dashboard");
    assert_eq!(event.requester.as_deref(), Some("github:1234"));

    for (body, why) in [
        (
            json!({"kind": "lunch", "url": "https://github.com/docspec/app/pull/7"}),
            "kind",
        ),
        (json!({"kind": "review", "url": "not a url"}), "url"),
        (
            json!({"url": "https://github.com/docspec/app/pull/7"}),
            "no kind",
        ),
    ] {
        let refused = act(&f, "/dashboard/api/v1/runs", &cookie, &csrf, &body).await;
        assert!(
            refused.status.is_client_error(),
            "{why}: {} {}",
            refused.status,
            refused.body
        );
    }
}

#[tokio::test]
async fn cancelling_through_the_api_fires_the_token_or_says_it_is_not_running_here() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (cookie, csrf) = viewer(&f);
    let running = RunId::parse("r-review").unwrap();

    let not_here = act(
        &f,
        "/dashboard/api/v1/runs/r-review/cancel",
        &cookie,
        &csrf,
        &json!({}),
    )
    .await;
    assert_eq!(not_here.status, StatusCode::CONFLICT);
    assert_eq!(not_here.code(), "conflict");

    let token = tokio_util::sync::CancellationToken::new();
    let _cancellable = f
        .dashboard
        .app
        .cancels
        .register(running.clone(), token.clone());
    let cancelled = act(
        &f,
        "/dashboard/api/v1/runs/r-review/cancel",
        &cookie,
        &csrf,
        &json!({}),
    )
    .await;
    assert_eq!(cancelled.status, StatusCode::ACCEPTED, "{}", cancelled.body);
    assert_eq!(cancelled.json(), json!({"run_id": "r-review"}));
    assert!(token.is_cancelled());
    let requests = f
        .dashboard
        .app
        .store
        .inbound_events_for_run(&running)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "cancel_requested")
        .count();
    assert_eq!(requests, 2, "both requests are recorded");
    assert_eq!(
        f.dashboard
            .app
            .store
            .run(&running)
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Running,
        "the run ends itself once its token fires"
    );
}

#[tokio::test]
async fn health_answers_and_no_response_holds_a_secret() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_review_record(&f).await;
    let (cookie, _) = viewer(&f);

    let health = get(&f, "/dashboard/api/v1/health", &cookie).await.json();
    let database = health["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "database")
        .unwrap();
    assert_eq!(database["state"], "ok");

    let secrets = ["csecret", &"k".repeat(32)];
    for (method, uri) in ROUTES {
        if *method != "GET" {
            continue;
        }
        let answer = get(&f, uri, &cookie).await;
        assert_eq!(answer.status, StatusCode::OK, "{uri}: {}", answer.body);
        for secret in secrets {
            assert!(!answer.body.contains(secret), "{uri} shows a secret");
        }
    }
}

/// The health row named `name`: its state and detail.
async fn health_row(f: &Fixture, cookie: &str, name: &str) -> (String, String) {
    let health = get(f, "/dashboard/api/v1/health", cookie).await.json();
    let row = health["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} row: {health}"))
        .clone();
    (
        row["state"].as_str().unwrap().to_owned(),
        row["detail"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn the_health_page_says_when_reviews_go_without_their_workspaces() {
    let quiet = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&quiet);
    assert_eq!(
        health_row(&quiet, &cookie, "workspaces").await,
        (
            "ok".to_owned(),
            "reviews run without workspaces: no profile has review = true".to_owned()
        )
    );
    assert_eq!(health_row(&quiet, &cookie, "git").await.0, "ok");

    let f = fixture_with(
        "https://127.0.0.1:9",
        crate::dashboard::app::Assets(&[]),
        |s| {
            s.workspace.default.backend = henk_domain::workspace::BackendKind::Kubernetes;
            s.workspace.default.review = true;
        },
    );
    let (cookie, _) = viewer(&f);
    let store = &f.dashboard.app.store;
    for (run, checked_out) in [("r-1", true), ("r-2", false), ("r-3", false)] {
        let id = RunId::parse(run).unwrap();
        store
            .create_run(&NewRun {
                id: id.clone(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "docspec/app".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "opened".into(),
                link: String::new(),
            })
            .await
            .unwrap();
        if checked_out {
            store
                .stage(
                    &id,
                    &henk_store::StageWrite::now(
                        henk_store::Stage::Checkout,
                        henk_store::StageState::Done,
                        "",
                    ),
                )
                .await
                .unwrap();
        } else {
            store
                .event(
                    &id,
                    "warn",
                    "review workspaces: none, the reviewed commit could not be checked out: git could not run: No such file or directory (os error 2)",
                )
                .await
                .unwrap();
            store
                .stage(
                    &id,
                    &henk_store::StageWrite::now(
                        henk_store::Stage::Checkout,
                        henk_store::StageState::Failed,
                        "x",
                    ),
                )
                .await
                .unwrap();
        }
        store
            .finish_run(&id, RunStatus::Finished, None, None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        health_row(&f, &cookie, "workspaces").await,
        (
            "warn".to_owned(),
            "1 of the last 3 reviews had one (checkout failed: git could not run: No such file or directory (os error 2))".to_owned()
        )
    );
    let (state, detail) = health_row(&f, &cookie, "git").await;
    assert_eq!(state, "ok", "git runs on the test machine: {detail}");
    assert!(
        detail.contains("needed for review workspaces (profile default)"),
        "{detail}"
    );
}

#[tokio::test]
async fn reviews_that_never_reach_their_checkout_do_not_count_as_going_without() {
    let f = fixture_with(
        "https://127.0.0.1:9",
        crate::dashboard::app::Assets(&[]),
        |s| {
            s.workspace.default.backend = henk_domain::workspace::BackendKind::Kubernetes;
            s.workspace.default.review = true;
        },
    );
    let (cookie, _) = viewer(&f);
    let store = &f.dashboard.app.store;
    // A review with nothing to review skips its checkout; one cancelled in
    // the queue ends with no checkout stage at all.
    let end = |run: &'static str, checkout: Option<henk_store::StageState>, status: RunStatus| {
        let store = store.clone();
        async move {
            let id = RunId::parse(run).unwrap();
            store
                .create_run(&NewRun {
                    id: id.clone(),
                    kind: RunKind::Review,
                    platform: Platform::GitHub,
                    repo: "docspec/app".into(),
                    target: 7,
                    commit: None,
                    requester: None,
                    trigger: "opened".into(),
                    link: String::new(),
                })
                .await
                .unwrap();
            if checkout == Some(henk_store::StageState::Failed) {
                store
                    .event(
                        &id,
                        "warn",
                        "review workspaces: none, the reviewed commit could not be checked out: fetch refused",
                    )
                    .await
                    .unwrap();
            }
            if let Some(state) = checkout {
                store
                    .stage(
                        &id,
                        &henk_store::StageWrite::now(henk_store::Stage::Checkout, state, ""),
                    )
                    .await
                    .unwrap();
            }
            store.finish_run(&id, status, None, None).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    end(
        "r-1",
        Some(henk_store::StageState::Skipped),
        RunStatus::Finished,
    )
    .await;
    end("r-2", None, RunStatus::Cancelled).await;
    assert_eq!(
        health_row(&f, &cookie, "workspaces").await,
        (
            "ok".to_owned(),
            "no review with workspaces has reached its checkout yet".to_owned()
        )
    );

    end(
        "r-3",
        Some(henk_store::StageState::Failed),
        RunStatus::Finished,
    )
    .await;
    end(
        "r-4",
        Some(henk_store::StageState::Skipped),
        RunStatus::Finished,
    )
    .await;
    end("r-5", None, RunStatus::Cancelled).await;
    assert_eq!(
        health_row(&f, &cookie, "workspaces").await,
        (
            "warn".to_owned(),
            "0 of the last 1 reviews had one (checkout failed: fetch refused)".to_owned()
        )
    );
}

/// The SPA's TypeScript types, generated from the API's./// The SPA's TypeScript types, generated from the API's. `HENK_BLESS=1`
/// writes them; otherwise a difference fails, so the checked-in file
/// always matches the API.
#[test]
fn api_types_are_current() {
    use ts_rs::{Config, TS};
    let cfg = Config::new().with_large_int("number");
    let decls = [
        types::Me::decl(&cfg),
        types::Page::<types::RunSummary>::decl(&cfg),
        types::RunSummary::decl(&cfg),
        types::RunCount::decl(&cfg),
        types::RunUpdate::decl(&cfg),
        types::QualityRow::decl(&cfg),
        types::QualitySeries::decl(&cfg),
        types::DayRate::decl(&cfg),
        types::DraftCount::decl(&cfg),
        types::ToolSummaryRow::decl(&cfg),
        types::ToolCallItem::decl(&cfg),
        types::DraftItem::decl(&cfg),
        types::RunningSnapshot::decl(&cfg),
        types::RunMessage::decl(&cfg),
        types::RunningMessage::decl(&cfg),
        types::RunDetail::decl(&cfg),
        types::LaneDot::decl(&cfg),
        types::StageDot::decl(&cfg),
        types::Progress::decl(&cfg),
        types::Slots::decl(&cfg),
        types::WaitingReview::decl(&cfg),
        types::OverviewStats::decl(&cfg),
        types::DayStats::decl(&cfg),
        types::LaneStats::decl(&cfg),
        types::SessionMessage::decl(&cfg),
        types::LiveSnapshot::decl(&cfg),
        types::LiveMessage::decl(&cfg),
        types::SessionEnd::decl(&cfg),
        types::ReviewMark::decl(&cfg),
        types::LaneRow::decl(&cfg),
        types::LaneOutcome::decl(&cfg),
        types::LaneReasons::decl(&cfg),
        types::Stage::decl(&cfg),
        types::Heartbeat::decl(&cfg),
        types::Lane::decl(&cfg),
        types::Finding::decl(&cfg),
        types::Draft::decl(&cfg),
        types::DraftDecision::decl(&cfg),
        types::TranscriptRef::decl(&cfg),
        types::ToolUsageRow::decl(&cfg),
        types::RunEvent::decl(&cfg),
        types::ToolCall::decl(&cfg),
        types::Transcript::decl(&cfg),
        types::Message::decl(&cfg),
        types::Part::decl(&cfg),
        types::EventSummary::decl(&cfg),
        types::EventItem::decl(&cfg),
        types::EventDetail::decl(&cfg),
        types::EventFacets::decl(&cfg),
        types::ListenerOutcome::decl(&cfg),
        types::Health::decl(&cfg),
        types::HealthCheck::decl(&cfg),
        types::StartRequest::decl(&cfg),
        types::Started::decl(&cfg),
        types::Cancelled::decl(&cfg),
        types::ErrorBody::decl(&cfg),
        types::ErrorDetail::decl(&cfg),
    ];
    let mut file = String::from(
        "// Generated from crates/henk/src/dashboard/api/types.rs by the test\n// api_types_are_current. Do not edit; run it with HENK_BLESS=1 instead.\n",
    );
    for decl in decls {
        file.push('\n');
        file.push_str("export ");
        file.push_str(decl.trim());
        file.push('\n');
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../dashboard/src/lib/api/types.ts");
    if std::env::var_os("HENK_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &file).unwrap();
        return;
    }
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        current == file,
        "{} is out of date with the API's types; run this test with HENK_BLESS=1",
        path.display()
    );
}

#[tokio::test]
async fn an_address_run_is_startable_only_where_address_runs_are_configured() {
    let f = fixture_with(
        "https://127.0.0.1:9",
        crate::dashboard::app::Assets(&[]),
        |s| {
            s.address = Some(toml::from_str("model = \"m\"\nrequester_id = 3").unwrap());
        },
    );
    let (cookie, _) = viewer(&f);
    let me = get(&f, "/dashboard/api/v1/me", &cookie).await.json();
    assert_eq!(me["startable"], json!(["review", "plan", "address"]));
}

#[tokio::test]
async fn the_count_spans_every_page_and_takes_the_filters() {
    let f = fixture("https://127.0.0.1:9");
    many_runs(&f, 120).await;
    let (cookie, _) = viewer(&f);
    let count = |query: &'static str| {
        let f = &f;
        let cookie = cookie.clone();
        async move {
            let answer = get(f, &format!("/dashboard/api/v1/runs/count{query}"), &cookie).await;
            assert_eq!(answer.status, StatusCode::OK, "{query}: {}", answer.body);
            answer.json()["count"].as_u64().unwrap()
        }
    };
    assert_eq!(count("").await, 120, "beyond one page of 100");
    assert_eq!(count("?status=running").await, 120);
    assert_eq!(count("?kind=plan").await, 12);
    assert_eq!(count("?target=42").await, 1);
    assert_eq!(count("?repo=docspec/other").await, 0);
    let bad = get(&f, "/dashboard/api/v1/runs/count?kind=lunch", &cookie).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

/// One Server-Sent Event as a test reads it.
#[derive(Debug)]
struct Message {
    event: String,
    data: Value,
    id: Option<String>,
}

/// Reads a stream's events one by one.
struct EventStream {
    body: Body,
    buffer: String,
}

impl EventStream {
    /// The next event; `None` when the stream has closed. Keep-alive
    /// comments are skipped. Fails the test after two seconds of nothing.
    async fn next(&mut self) -> Option<Message> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                let (mut event, mut data, mut id) = (String::from("message"), String::new(), None);
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_owned();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    } else if let Some(v) = line.strip_prefix("id:") {
                        id = Some(v.trim().to_owned());
                    }
                }
                if data.is_empty() {
                    continue;
                }
                return Some(Message {
                    event,
                    data: serde_json::from_str(&data).unwrap(),
                    id,
                });
            }
            let frame = tokio::time::timeout(Duration::from_secs(2), self.body.frame())
                .await
                .expect("the stream said nothing for two seconds");
            match frame {
                Some(frame) => {
                    if let Ok(bytes) = frame.unwrap().into_data() {
                        self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
                None => return None,
            }
        }
    }

    /// The events up to and including `end`, by name.
    async fn names_until_end(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        while let Some(message) = self.next().await {
            names.push(message.event.clone());
            if message.event == "end" {
                break;
            }
        }
        names
    }
}

async fn stream(f: &Fixture, uri: &str, cookie: &str, last: Option<&str>) -> EventStream {
    let mut request = Request::get(uri).header("cookie", cookie);
    if let Some(last) = last {
        request = request.header("last-event-id", last);
    }
    let response = f
        .router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    EventStream {
        body: response.into_body(),
        buffer: String::new(),
    }
}

/// A running review on this process, as the coordinator would start one.
async fn live_run(f: &Fixture, run: &str) -> (RunId, crate::liveness::KeepAlive) {
    let id = RunId::parse(run).unwrap();
    f.dashboard
        .app
        .store
        .create_run(&NewRun {
            id: id.clone(),
            kind: RunKind::Review,
            platform: Platform::GitHub,
            repo: "docspec/app".into(),
            target: 7,
            commit: Some("abc".into()),
            requester: None,
            trigger: "opened".into(),
            link: String::new(),
        })
        .await
        .unwrap();
    let alive = crate::liveness::KeepAlive::start(
        Arc::clone(&f.dashboard.app.store),
        &f.dashboard.app.live_runs,
        id.clone(),
    );
    (id, alive)
}

fn tool_call(turn: u32) -> ToolCallRecord {
    ToolCallRecord {
        at: String::new(),
        session: "lane-a".into(),
        model: "model-x".into(),
        turn,
        tool: "read_file".into(),
        origin: "henk".into(),
        outcome: "ok".into(),
        arguments: "{}".into(),
        arguments_len: 2,
        result_chars: 10,
        elapsed_ms: 1,
    }
}

#[tokio::test]
async fn a_running_run_streams_what_happens_in_order_and_ends() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&f);
    let (run, _alive) = live_run(&f, "r-live").await;
    let store = Arc::clone(&f.dashboard.app.store);
    let mut events = stream(&f, "/dashboard/api/v1/runs/r-live/stream", &cookie, None).await;

    let snapshot = events.next().await.unwrap();
    assert_eq!(snapshot.event, "snapshot");
    assert_eq!(snapshot.data["run"]["status"], "running");
    assert!(
        snapshot
            .id
            .unwrap()
            .starts_with(f.dashboard.app.feed.epoch())
    );

    store.start_lane(&run, "lane-a", "model-x").await.unwrap();
    store.record_tool_call(&run, &tool_call(4)).await.unwrap();
    store
        .record_draft(
            &run,
            &DraftRecord {
                at: String::new(),
                draft: "d1".into(),
                lane: "lane-a".into(),
                model: "model-x".into(),
                kind: "finding".into(),
                path: "src/a.rs".into(),
                line: 4,
                target: String::new(),
                body: "<b>Wrong</b>.".into(),
                decision: None,
            },
        )
        .await
        .unwrap();
    store
        .decide_draft(
            &run,
            "d1",
            &DraftDecision {
                at: String::new(),
                verdict: DraftVerdict::Rejected,
                checker: "model-y".into(),
                reason: "It is right.".into(),
                same_as: String::new(),
                comment_id: String::new(),
            },
        )
        .await
        .unwrap();
    store
        .finish_lane(
            &run,
            "lane-a",
            henk_store::LaneStatus::Finished,
            4,
            100,
            9,
            None,
        )
        .await
        .unwrap();
    store.event(&run, "info", "lane-a: EndTurn").await.unwrap();
    store
        .finish_run(&run, RunStatus::Finished, Some("Not bad."), None)
        .await
        .unwrap();

    let mut seen = Vec::new();
    while let Some(message) = events.next().await {
        seen.push(message);
    }
    let names: Vec<&str> = seen.iter().map(|m| m.event.as_str()).collect();
    assert_eq!(
        names,
        [
            "lanes",
            "tool_call",
            "draft",
            "draft",
            "lanes",
            "event",
            "run",
            "end"
        ],
        "in order, and the stream closes after end"
    );
    assert_eq!(seen[0].data[0]["name"], "lane-a");
    assert_eq!(seen[1].data["turn"], 4);
    assert_eq!(seen[2].data["body"], "<b>Wrong</b>.", "text as stored");
    assert_eq!(seen[3].data["decision"]["verdict"], "rejected");
    assert_eq!(seen[4].data[0]["turns"], 4);
    assert_eq!(seen[6].data["run"]["status"], "finished");
    assert_eq!(seen[6].data["summary"], "Not bad.");
    let ids: Vec<u64> = seen[..7]
        .iter()
        .map(|m| {
            m.id.as_deref()
                .unwrap()
                .rsplit_once('-')
                .unwrap()
                .1
                .parse()
                .unwrap()
        })
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
}

#[tokio::test]
async fn a_reconnect_gets_exactly_what_it_missed_or_a_snapshot() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&f);
    let (run, _alive) = live_run(&f, "r-live").await;
    let store = Arc::clone(&f.dashboard.app.store);
    let uri = "/dashboard/api/v1/runs/r-live/stream";

    let mut first = stream(&f, uri, &cookie, None).await;
    first.next().await.unwrap();
    store.start_lane(&run, "lane-a", "model-x").await.unwrap();
    let seen = first.next().await.unwrap();
    assert_eq!(seen.event, "lanes");
    drop(first);

    store.record_tool_call(&run, &tool_call(1)).await.unwrap();
    store.event(&run, "info", "missed").await.unwrap();
    let other = live_run(&f, "r-other").await;
    store.event(&other.0, "info", "another run").await.unwrap();

    let mut again = stream(&f, uri, &cookie, seen.id.as_deref()).await;
    let replayed = [again.next().await.unwrap(), again.next().await.unwrap()];
    assert_eq!(replayed[0].event, "tool_call");
    assert_eq!(replayed[1].event, "event");
    assert_eq!(replayed[1].data["message"], "missed");

    let mut fresh = stream(&f, uri, &cookie, Some("0123456789ab-3")).await;
    let snapshot = fresh.next().await.unwrap();
    assert_eq!(snapshot.event, "snapshot", "another process's id");
    assert_eq!(snapshot.data["lanes"][0]["last_call_turn"], 1);
    assert_eq!(snapshot.data["events"][0]["message"], "missed");
}

#[tokio::test]
async fn a_run_that_ends_after_catching_up_is_shown_ended_before_end() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&f);
    let (run, _alive) = live_run(&f, "r-live").await;
    let store = Arc::clone(&f.dashboard.app.store);
    store
        .finish_run(&run, RunStatus::Finished, Some("Not bad."), None)
        .await
        .unwrap();
    // Up to date with the feed but not shown the run ended, as when it
    // ends between catching up and following: the replay is empty and the
    // store says it ended.
    let feed = &f.dashboard.app.feed;
    let last = format!("{}-{}", feed.epoch(), feed.last());
    let uri = "/dashboard/api/v1/runs/r-live/stream";
    let mut events = stream(&f, uri, &cookie, Some(&last)).await;
    let ended = events.next().await.unwrap();
    assert_eq!(ended.event, "snapshot", "the run as it ended, not only end");
    assert_eq!(ended.data["run"]["status"], "finished");
    assert_eq!(ended.data["summary"], "Not bad.");
    assert_eq!(events.names_until_end().await, ["end"]);
}

#[tokio::test]
async fn an_ended_run_streams_its_snapshot_and_ends() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (cookie, _) = viewer(&f);
    let mut events = stream(&f, "/dashboard/api/v1/runs/r-plan/stream", &cookie, None).await;
    assert_eq!(events.names_until_end().await, ["snapshot", "end"]);
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn a_run_of_another_process_is_read_again_until_it_ends() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (cookie, _) = viewer(&f);
    let run = RunId::parse("r-review").unwrap();
    assert!(!f.dashboard.app.live_runs.contains(&run));
    let mut events = stream(&f, "/dashboard/api/v1/runs/r-review/stream", &cookie, None).await;
    assert_eq!(events.next().await.unwrap().event, "snapshot");
    let again = events.next().await.unwrap();
    assert_eq!(again.event, "snapshot", "read again from the store");
    assert_eq!(again.data["run"]["status"], "running");
    f.dashboard
        .app
        .store
        .finish_run(&run, RunStatus::Failed, None, Some("boom"))
        .await
        .unwrap();
    let mut last = None;
    while let Some(message) = events.next().await {
        if message.event == "end" {
            break;
        }
        last = Some(message);
    }
    let last = last.unwrap();
    assert_eq!(last.data["run"]["status"], "failed");
    assert_eq!(last.data["error"], "boom");
}

#[tokio::test]
async fn the_running_stream_says_what_runs_and_what_starts_and_ends() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (cookie, _) = viewer(&f);
    let mut events = stream(&f, "/dashboard/api/v1/runs/stream", &cookie, None).await;
    let snapshot = events.next().await.unwrap();
    assert_eq!(snapshot.event, "snapshot");
    assert_eq!(snapshot.data["count"], 1);
    assert_eq!(snapshot.data["runs"][0]["id"], "r-review");

    let (run, _alive) = live_run(&f, "r-new").await;
    let mut started = events.next().await.unwrap();
    while started.event == "snapshot" {
        started = events.next().await.unwrap();
    }
    assert_eq!(started.event, "run");
    assert_eq!(started.data["id"], "r-new");
    assert_eq!(started.data["status"], "running");
    f.dashboard
        .app
        .store
        .finish_run(&run, RunStatus::Finished, None, None)
        .await
        .unwrap();
    let mut ended = events.next().await.unwrap();
    while ended.event == "snapshot" {
        ended = events.next().await.unwrap();
    }
    assert_eq!(ended.data["id"], "r-new");
    assert_eq!(ended.data["status"], "finished");
}

#[tokio::test]
async fn the_running_stream_carries_slots_and_a_runs_lanes_and_stages_moving_on() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (cookie, _) = viewer(&f);
    let review = RunId::parse("r-review").unwrap();
    let store = &f.dashboard.app.store;
    store.start_lane(&review, "lane-x", "m").await.unwrap();
    let mut events = stream(&f, "/dashboard/api/v1/runs/stream", &cookie, None).await;
    let snapshot = events.next().await.unwrap();
    assert_eq!(snapshot.event, "snapshot");
    assert!(snapshot.data["slots"]["limit"].as_u64().unwrap() >= 1);
    assert_eq!(snapshot.data["slots"]["waiting"], json!([]));
    let lanes = &snapshot.data["runs"][0]["lanes"];
    assert!(
        lanes
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "lane-x" && l["status"] == "running"),
        "{lanes}"
    );

    store
        .stage(
            &review,
            &henk_store::StageWrite::now(
                henk_store::Stage::Diff,
                henk_store::StageState::Done,
                "1 file",
            ),
        )
        .await
        .unwrap();
    let mut moved = events.next().await.unwrap();
    while moved.event == "snapshot" {
        moved = events.next().await.unwrap();
    }
    assert_eq!(moved.event, "progress");
    assert_eq!(moved.data["run_id"], "r-review");
    assert_eq!(moved.data["lanes"], Value::Null);
    assert_eq!(
        moved.data["stages"],
        json!([{"name": "diff", "state": "done"}])
    );
}

#[tokio::test]
async fn the_running_stream_sends_the_slots_when_they_change_not_only_in_snapshots() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&f);
    let mut events = stream(&f, "/dashboard/api/v1/runs/stream", &cookie, None).await;
    let snapshot = events.next().await.unwrap();
    assert_eq!(snapshot.event, "snapshot");

    let request = crate::review::ReviewRequest {
        target: henk_platform::ReviewTarget {
            repo: henk_domain::allowlist::RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
            number: 8,
        },
        commit: None,
        trigger: "test".to_owned(),
        requester: None,
        acknowledge: None,
        run: None,
        submitted_at: None,
    };
    let commit =
        henk_domain::review::CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap();
    let _ = f.dashboard.coordinator.submit_review(request, commit);
    // The review cannot reach a platform and ends at once: its slot is
    // taken and freed, and the stream says so without a snapshot.
    let freed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let message = events.next().await.unwrap();
            if message.event == "slots"
                && message.data["in_use"] == 0
                && message.data["waiting"] == json!([])
            {
                return message;
            }
        }
    })
    .await
    .expect("a slots message after the review freed its slot");
    assert!(freed.data["limit"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn the_overview_counts_each_day_and_lists_runs_with_their_lanes() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (cookie, _) = viewer(&f);
    let stats = get(&f, "/dashboard/api/v1/stats/overview?days=3", &cookie).await;
    assert_eq!(stats.status, StatusCode::OK, "{}", stats.body);
    let stats = stats.json();
    let days = stats["days"].as_array().unwrap();
    assert_eq!(days.len(), 3);
    assert_eq!(days[2]["day"], stats["to"]);
    assert_eq!(days[0]["day"], stats["from"]);
    let runs: u64 = days.iter().map(|d| d["runs"].as_u64().unwrap()).sum();
    assert_eq!(runs, 2, "the two seeded runs started today");
    for bad in ["0", "91", "x"] {
        let answer = get(
            &f,
            &format!("/dashboard/api/v1/stats/overview?days={bad}"),
            &cookie,
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let default = get(&f, "/dashboard/api/v1/stats/overview", &cookie)
        .await
        .json();
    assert_eq!(default["days"].as_array().unwrap().len(), 14);

    let review = RunId::parse("r-review").unwrap();
    f.dashboard
        .app
        .store
        .start_lane(&review, "lane-x", "m")
        .await
        .unwrap();
    let listed = get(&f, "/dashboard/api/v1/runs", &cookie).await.json();
    let item = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "r-review")
        .unwrap()
        .clone();
    assert!(
        item["lanes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "lane-x"),
        "{item}"
    );
    assert!(item["stages"].is_array());
}

#[tokio::test]
async fn lane_reliability_shows_the_ended_reviews_lanes_and_their_limits() {
    let f = fixture("https://127.0.0.1:9");
    let store = &f.dashboard.app.store;
    for (run, lane_status, error) in [
        ("r-1", LaneStatus::Finished, None),
        (
            "r-2",
            LaneStatus::DidNotFinish,
            Some("rate limited by the model endpoint"),
        ),
    ] {
        let id = RunId::parse(run).unwrap();
        store
            .create_run(&NewRun {
                id: id.clone(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "docspec/app".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "opened".into(),
                link: format!("https://henk.example/runs/{run}"),
            })
            .await
            .unwrap();
        store.start_lane(&id, "lane-a", "m").await.unwrap();
        store
            .finish_lane(&id, "lane-a", lane_status, 1, 1, 1, error)
            .await
            .unwrap();
        store
            .finish_run(&id, RunStatus::Finished, None, None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let (cookie, _) = viewer(&f);
    let answer = get(&f, "/dashboard/api/v1/stats/lanes", &cookie).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let stats = answer.json();
    assert_eq!(stats["reviews"][0]["run_id"], "r-1", "oldest first");
    let lane = &stats["lanes"][0];
    assert_eq!(lane["name"], "lane-a");
    assert_eq!(lane["did_not_finish"], 1);
    assert_eq!(lane["reasons"]["rate_limit"], 1);
    assert_eq!(lane["outcomes"][1]["status"], "did_not_finish");
    assert_eq!(lane["outcomes"][1]["reason"], "rate_limit");
    assert_eq!(lane["outcomes"][0]["reason"], Value::Null);

    let newest = get(&f, "/dashboard/api/v1/stats/lanes?last=1", &cookie)
        .await
        .json();
    assert_eq!(newest["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(newest["reviews"][0]["run_id"], "r-2");
    let since = get(
        &f,
        "/dashboard/api/v1/stats/lanes?since=2000-01-01T00:00:00Z",
        &cookie,
    )
    .await
    .json();
    assert_eq!(since["reviews"].as_array().unwrap().len(), 2);
    for bad in [
        "last=0",
        "last=201",
        "last=x",
        "since=yesterday",
        "last=5&since=2000-01-01T00:00:00Z",
    ] {
        let answer = get(&f, &format!("/dashboard/api/v1/stats/lanes?{bad}"), &cookie).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let refused = call(&f, Method::GET, "/dashboard/api/v1/stats/lanes", &[], None).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_event_filters_are_the_sources_and_kinds_recorded() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let (cookie, _) = viewer(&f);
    let facets = get(&f, "/dashboard/api/v1/events/facets", &cookie).await;
    assert_eq!(facets.status, StatusCode::OK, "{}", facets.body);
    let facets = facets.json();
    let sources = facets["sources"].as_array().unwrap();
    assert!(!sources.is_empty());
    let mut sorted = sources.clone();
    sorted.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    assert_eq!(*sources, sorted, "sorted");
    assert!(facets["kinds"].is_array());
    let refused = call(
        &f,
        Method::GET,
        "/dashboard/api/v1/events/facets",
        &[],
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
}

fn said(message: &henk_llm::ChatMessage) -> String {
    serde_json::to_string(message).unwrap()
}

#[tokio::test]
async fn a_running_session_streams_what_it_says_as_text_and_ends_with_it() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&f);
    let (run, _alive) = live_run(&f, "r-live").await;
    let store = Arc::clone(&f.dashboard.app.store);
    store.start_lane(&run, "lane-a", "model-x").await.unwrap();
    let opening = henk_llm::ChatMessage::user("<script>alert(1)</script> Review this.");
    store
        .session_message(&run, "lane-a", 0, &said(&opening))
        .await;
    let uri = "/dashboard/api/v1/runs/r-live/sessions/lane-a/stream";
    let mut events = stream(&f, uri, &cookie, None).await;

    let snapshot = events.next().await.unwrap();
    assert_eq!(snapshot.event, "snapshot");
    let epoch = f.dashboard.app.feed.epoch().to_owned();
    assert_eq!(snapshot.id.as_deref(), Some(format!("{epoch}-1").as_str()));
    assert_eq!(snapshot.data["cut"], false);
    let first = &snapshot.data["messages"][0];
    assert_eq!(first["seq"], 1);
    assert_eq!(first["message"]["role"], "user");
    assert_eq!(first["message"]["turn"], 0);
    assert_eq!(
        first["message"]["parts"][0]["text"], "<script>alert(1)</script> Review this.",
        "text, as it was said"
    );

    let answer = henk_llm::ChatMessage::assistant("Looking.");
    store
        .session_message(&run, "lane-a", 1, &said(&answer))
        .await;
    store
        .session_message(&run, "lane-b", 1, &said(&answer))
        .await;
    let next = events.next().await.unwrap();
    assert_eq!(next.event, "message");
    assert_eq!(next.id.as_deref(), Some(format!("{epoch}-2").as_str()));
    assert_eq!(next.data["message"]["parts"][0]["text"], "Looking.");
    assert_eq!(next.data["message"]["turn"], 1);

    // A follower that saw the first message gets only the second.
    let mut again = stream(&f, uri, &cookie, Some(&format!("{epoch}-1"))).await;
    let replayed = again.next().await.unwrap();
    assert_eq!(
        (replayed.event.as_str(), replayed.data["seq"].as_u64()),
        ("message", Some(2))
    );
    // One from another process, or before a restart, starts again.
    let mut other = stream(&f, uri, &cookie, Some("0badf00d-1")).await;
    assert_eq!(other.next().await.unwrap().event, "snapshot");

    store
        .finish_lane(&run, "lane-a", LaneStatus::Finished, 1, 1, 1, None)
        .await
        .unwrap();
    let end = events.next().await.unwrap();
    assert_eq!(end.event, "end");
    assert_eq!(end.data["elsewhere"], false);
    assert!(events.next().await.is_none(), "the stream closes");
    assert_eq!(again.names_until_end().await, ["end"]);

    // Ended now: the transcript takes over.
    let mut late = stream(&f, uri, &cookie, None).await;
    assert_eq!(late.names_until_end().await, ["end"]);
}

#[tokio::test]
async fn a_session_another_process_runs_ends_its_stream_at_once() {
    let f = fixture("https://127.0.0.1:9");
    let (cookie, _) = viewer(&f);
    let store = Arc::clone(&f.dashboard.app.store);
    let run = RunId::parse("r-there").unwrap();
    store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Review,
            platform: Platform::GitHub,
            repo: "docspec/app".into(),
            target: 7,
            commit: None,
            requester: None,
            trigger: "opened".into(),
            link: String::new(),
        })
        .await
        .unwrap();
    store.start_lane(&run, "lane-a", "model-x").await.unwrap();
    let mut events = stream(
        &f,
        "/dashboard/api/v1/runs/r-there/sessions/lane-a/stream",
        &cookie,
        None,
    )
    .await;
    let end = events.next().await.unwrap();
    assert_eq!(
        (end.event.as_str(), &end.data["elsewhere"]),
        ("end", &json!(true))
    );
    let missing = get(
        &f,
        "/dashboard/api/v1/runs/r-none/sessions/lane-a/stream",
        &cookie,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn no_stream_without_a_session() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    for uri in [
        "/dashboard/api/v1/runs/stream",
        "/dashboard/api/v1/runs/r-review/stream",
        "/dashboard/api/v1/runs/r-review/sessions/lane-a/stream",
    ] {
        let answer = call(&f, Method::GET, uri, &[], None).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (cookie, _) = viewer(&f);
    let missing = get(&f, "/dashboard/api/v1/runs/r-none/stream", &cookie).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

/// Drafts on `r-review` and `r-plan` (seeded by `seed`): mistral rejected
/// three times and confirmed once on r-review, deepseek confirmed once,
/// repeated once and is waiting once on r-plan. All on 2026-10-07.
async fn seed_drafts(f: &Fixture) {
    seed_drafts_on(f, "2026-10-07").await;
}

/// The drafts of [`seed_drafts`] on `day` (`YYYY-MM-DD`).
async fn seed_drafts_on(f: &Fixture, day: &str) {
    let store = &f.dashboard.app.store;
    let drafts: [(&str, &str, &str, &str, Option<DraftVerdict>); 7] = [
        (
            "r-review",
            "d1",
            "lane-a",
            "mistral",
            Some(DraftVerdict::Rejected),
        ),
        (
            "r-review",
            "d2",
            "lane-a",
            "mistral",
            Some(DraftVerdict::Rejected),
        ),
        (
            "r-review",
            "d3",
            "lane-a",
            "mistral",
            Some(DraftVerdict::Rejected),
        ),
        (
            "r-review",
            "d4",
            "lane-a",
            "mistral",
            Some(DraftVerdict::Confirmed),
        ),
        (
            "r-plan",
            "d1",
            "lane-b",
            "deepseek",
            Some(DraftVerdict::Confirmed),
        ),
        (
            "r-plan",
            "d2",
            "lane-b",
            "deepseek",
            Some(DraftVerdict::SameAs),
        ),
        ("r-plan", "d3", "lane-b", "deepseek", None),
    ];
    for (minute, (run, draft, lane, model, verdict)) in (0u32..).zip(drafts) {
        let run = RunId::parse(run).unwrap();
        let at = format!("{day}T10:{minute:02}:00Z");
        store
            .record_draft(
                &run,
                &DraftRecord {
                    at: at.clone(),
                    draft: draft.into(),
                    lane: lane.into(),
                    model: model.into(),
                    kind: "finding".into(),
                    path: "src/a.rs".into(),
                    line: 4,
                    target: String::new(),
                    body: format!("{model} says <b>{draft}</b>"),
                    decision: None,
                },
            )
            .await
            .unwrap();
        if let Some(verdict) = verdict {
            store
                .decide_draft(
                    &run,
                    draft,
                    &DraftDecision {
                        at,
                        verdict,
                        checker: "opus".into(),
                        reason: "src/a.rs:4 says otherwise.".into(),
                        same_as: String::new(),
                        comment_id: String::new(),
                    },
                )
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn quality_counts_drafts_per_group_with_the_rejection_rate() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_drafts(&f).await;
    let (cookie, _) = viewer(&f);

    let by_model = get(&f, "/dashboard/api/v1/quality", &cookie).await;
    assert_eq!(by_model.status, StatusCode::OK, "{}", by_model.body);
    let by_model = by_model.json();
    assert_eq!(by_model[0]["key"], "mistral");
    assert_eq!(by_model[0]["drafts"], 4);
    assert_eq!(by_model[0]["rejected"], 3);
    assert_eq!(by_model[0]["judged"], 4);
    assert_eq!(by_model[0]["rejection_rate"], 0.75);
    assert_eq!(by_model[1]["key"], "deepseek");
    assert_eq!(by_model[1]["same_as"], 1);
    assert_eq!(by_model[1]["waiting"], 1);
    assert_eq!(by_model[1]["rejection_rate"], 0.0);

    let by_target = get(&f, "/dashboard/api/v1/quality?group=target", &cookie)
        .await
        .json();
    assert_eq!(by_target[0]["key"], "docspec/app #7");
    assert_eq!(by_target[0]["drafts"], 7);
    assert!(
        by_target[0]["target_url"]
            .as_str()
            .unwrap()
            .ends_with("/docspec/app/pull/7")
    );

    let none = get(
        &f,
        "/dashboard/api/v1/quality?group=lane&since=2030-01-01T00:00:00Z",
        &cookie,
    )
    .await
    .json();
    assert_eq!(none, json!([]));
    let waiting_only = get(
        &f,
        "/dashboard/api/v1/quality?group=lane&since=2026-10-07T10:06:00Z",
        &cookie,
    )
    .await
    .json();
    assert_eq!(waiting_only[0]["judged"], 0);
    assert_eq!(
        waiting_only[0]["rejection_rate"],
        Value::Null,
        "nothing judged"
    );

    for query in ["group=colour", "since=yesterday"] {
        let bad = get(&f, &format!("/dashboard/api/v1/quality?{query}"), &cookie).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(bad.code(), "bad_request");
    }
}

#[tokio::test]
async fn quality_has_a_rate_per_day_for_the_largest_groups_and_counts_drafts() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    // The chart ends today, so the drafts are seeded today and the test
    // does not depend on the date it runs on.
    let today = time::OffsetDateTime::now_utc().date().to_string();
    seed_drafts_on(&f, &today).await;
    let (cookie, _) = viewer(&f);
    let daily = get(&f, "/dashboard/api/v1/quality/daily?group=model", &cookie).await;
    assert_eq!(daily.status, StatusCode::OK, "{}", daily.body);
    let daily = daily.json();
    let series = daily.as_array().unwrap();
    assert_eq!(
        series
            .iter()
            .map(|s| s["key"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["mistral", "deepseek"],
        "the largest group first"
    );
    let days = series[0]["days"].as_array().unwrap();
    assert_eq!(days[0]["day"], today.as_str(), "from the first drafted day");
    let judged: u64 = days.iter().map(|d| d["judged"].as_u64().unwrap()).sum();
    let rejected: u64 = days.iter().map(|d| d["rejected"].as_u64().unwrap()).sum();
    assert_eq!((judged, rejected), (4, 3));
    assert!(
        days.iter()
            .all(|d| d["rate"].is_null() == (d["judged"] == 0))
    );
    let bad = get(&f, "/dashboard/api/v1/quality/daily?group=colour", &cookie).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    for query in ["verdict=rejected", "model=deepseek", "verdict=waiting", ""] {
        let counted = get(
            &f,
            &format!("/dashboard/api/v1/drafts/count?{query}"),
            &cookie,
        )
        .await
        .json();
        let listed = get(
            &f,
            &format!("/dashboard/api/v1/drafts?{query}&limit=100"),
            &cookie,
        )
        .await
        .json();
        assert_eq!(
            counted["count"].as_u64().unwrap(),
            listed["items"].as_array().unwrap().len() as u64,
            "{query}"
        );
    }
}

#[tokio::test]
async fn quality_daily_picks_the_largest_groups_of_the_days_it_draws() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    let store = &f.dashboard.app.store;
    let run = RunId::parse("r-review").unwrap();
    let today = time::OffsetDateTime::now_utc().date();
    let long_ago = today - time::Duration::days(100);
    // Seven busy models before the 90 days the chart draws, one quiet
    // model inside them.
    let mut drafts = Vec::new();
    for model in 0..7 {
        for _ in 0..3 {
            drafts.push((long_ago, format!("retired-{model}")));
        }
    }
    drafts.push((today, "recent".to_owned()));
    for (n, (day, model)) in drafts.into_iter().enumerate() {
        let at = format!("{day}T10:00:00Z");
        let draft = format!("d{n}");
        store
            .record_draft(
                &run,
                &DraftRecord {
                    at: at.clone(),
                    draft: draft.clone(),
                    lane: "lane-a".into(),
                    model,
                    kind: "finding".into(),
                    path: "src/a.rs".into(),
                    line: 4,
                    target: String::new(),
                    body: "a finding".into(),
                    decision: None,
                },
            )
            .await
            .unwrap();
        store
            .decide_draft(
                &run,
                &draft,
                &DraftDecision {
                    at,
                    verdict: DraftVerdict::Rejected,
                    checker: "opus".into(),
                    reason: "src/a.rs:4 says otherwise.".into(),
                    same_as: String::new(),
                    comment_id: String::new(),
                },
            )
            .await
            .unwrap();
    }
    let (cookie, _) = viewer(&f);
    let daily = get(&f, "/dashboard/api/v1/quality/daily?group=model", &cookie)
        .await
        .json();
    let keys: Vec<&str> = daily
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["recent"], "nothing the chart does not draw");
    assert_eq!(daily[0]["days"][0]["day"], today.to_string().as_str());
    assert_eq!(daily[0]["days"][0]["rejected"], 1);
}

#[tokio::test]
async fn drafts_list_across_runs_by_verdict_model_and_lane_a_page_at_a_time() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_drafts(&f).await;
    let (cookie, _) = viewer(&f);
    let bodies = |page: &Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["draft"]["body"].as_str().unwrap().to_owned())
            .collect()
    };

    let rejected = get(&f, "/dashboard/api/v1/drafts?verdict=rejected", &cookie)
        .await
        .json();
    assert_eq!(
        bodies(&rejected),
        [
            "mistral says <b>d3</b>",
            "mistral says <b>d2</b>",
            "mistral says <b>d1</b>"
        ],
        "newest first, text as stored"
    );
    let first = &rejected["items"][0];
    assert_eq!(first["run_id"], "r-review");
    assert_eq!(first["repo"], "docspec/app");
    assert_eq!(
        first["draft"]["decision"]["reason"],
        "src/a.rs:4 says otherwise."
    );
    assert!(first["target_url"].as_str().unwrap().ends_with("/pull/7"));

    let waiting = get(&f, "/dashboard/api/v1/drafts?verdict=waiting", &cookie)
        .await
        .json();
    assert_eq!(bodies(&waiting), ["deepseek says <b>d3</b>"]);
    let lane_b = get(
        &f,
        "/dashboard/api/v1/drafts?lane=lane-b&model=deepseek",
        &cookie,
    )
    .await
    .json();
    assert_eq!(bodies(&lane_b).len(), 3);

    let mut seen = Vec::new();
    let mut uri = "/dashboard/api/v1/drafts?limit=3".to_owned();
    loop {
        let page = get(&f, &uri, &cookie).await.json();
        seen.extend(bodies(&page));
        match page["next"].as_str() {
            Some(next) => uri = format!("/dashboard/api/v1/drafts?limit=3&cursor={next}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 7, "every draft once: {seen:?}");

    for query in ["verdict=maybe", "cursor=bm9wZQ"] {
        let bad = get(&f, &format!("/dashboard/api/v1/drafts?{query}"), &cookie).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}");
    }
}

/// Tool calls on `r-review` (lanes and a check) and `r-plan` (the
/// planner), seeded by `seed`: eight calls, four of them gone wrong.
async fn seed_calls(f: &Fixture) {
    let store = &f.dashboard.app.store;
    let calls = [
        (
            "r-review",
            "lane-a",
            "mistral",
            "read_file",
            "ok",
            "{\"path\":\"a.rs\"}",
        ),
        (
            "r-review",
            "lane-a",
            "mistral",
            "read_file",
            "ok",
            "{\"path\":\"b.rs\"}",
        ),
        (
            "r-review",
            "lane-a",
            "mistral",
            "read_file",
            "error",
            "{\"path\":\"<script>.rs\"}",
        ),
        (
            "r-review",
            "lane-a",
            "mistral",
            "github__get_file",
            "refused_scope",
            "{\"repo\":\"other/repo\"}",
        ),
        (
            "r-review",
            "lane-b",
            "deepseek",
            "read_file",
            "refused_repeat",
            "{\"path\":\"a.rs\"}",
        ),
        (
            "r-review",
            "check-1",
            "opus",
            "read_file",
            "ok",
            "{\"path\":\"a.rs\"}",
        ),
        (
            "r-plan",
            "planner",
            "deepseek",
            "bash",
            "ok",
            "{\"cmd\":\"ls\"}",
        ),
        (
            "r-plan",
            "planner",
            "deepseek",
            "bash",
            "malformed_arguments",
            "not json",
        ),
    ];
    for (n, (run, session, model, tool, outcome, arguments)) in (1u32..).zip(calls) {
        store
            .record_tool_call(
                &RunId::parse(run).unwrap(),
                &ToolCallRecord {
                    at: format!("2026-10-07T10:{n:02}:00Z"),
                    session: session.into(),
                    model: model.into(),
                    turn: n,
                    tool: tool.into(),
                    origin: "henk".into(),
                    outcome: outcome.into(),
                    arguments: arguments.into(),
                    arguments_len: 10,
                    result_chars: 100,
                    elapsed_ms: 10,
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_tool_summary_counts_per_tool_model_and_kind_with_rates() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_calls(&f).await;
    let (cookie, _) = viewer(&f);

    let rows = get(&f, "/dashboard/api/v1/tool-calls/summary", &cookie).await;
    assert_eq!(rows.status, StatusCode::OK, "{}", rows.body);
    let rows = rows.json();
    assert_eq!(rows[0]["tool"], "read_file");
    assert_eq!(rows[0]["model"], "mistral");
    assert_eq!(rows[0]["session_kind"], "lane");
    assert_eq!(rows[0]["calls"], 3);
    assert_eq!(rows[0]["errors"], 1);
    assert!((rows[0]["error_rate"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-9);
    assert_eq!(rows[0]["refusal_rate"], 0.0);
    assert_eq!(rows.as_array().unwrap().len(), 5);

    let planner = get(
        &f,
        "/dashboard/api/v1/tool-calls/summary?session_kind=planner",
        &cookie,
    )
    .await
    .json();
    assert_eq!(planner[0]["tool"], "bash");
    assert_eq!(planner[0]["calls"], 2);
    assert_eq!(planner[0]["other"], 1);
    let early = get(
        &f,
        "/dashboard/api/v1/tool-calls/summary?model=mistral&until=2026-10-07T10:02:00Z",
        &cookie,
    )
    .await
    .json();
    assert_eq!(early[0]["calls"], 1, "until is not inclusive");

    for query in ["session_kind=robot", "since=yesterday"] {
        let bad = get(
            &f,
            &format!("/dashboard/api/v1/tool-calls/summary?{query}"),
            &cookie,
        )
        .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}");
    }
}

#[tokio::test]
async fn the_calls_that_went_wrong_list_across_runs_a_page_at_a_time() {
    let f = fixture("https://127.0.0.1:9");
    seed(&f).await;
    seed_calls(&f).await;
    let (cookie, _) = viewer(&f);
    let outcomes = |page: &Value| -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["call"]["outcome"].as_str().unwrap().to_owned())
            .collect()
    };

    let problems = get(&f, "/dashboard/api/v1/tool-calls?outcome=problems", &cookie)
        .await
        .json();
    assert_eq!(
        outcomes(&problems),
        [
            "malformed_arguments",
            "refused_repeat",
            "refused_scope",
            "error"
        ],
        "newest first, everything but ok"
    );
    let first = &problems["items"][0];
    assert_eq!(first["run_id"], "r-plan");
    assert_eq!(first["call"]["arguments"], "not json");
    assert!(
        first["target_url"]
            .as_str()
            .unwrap()
            .ends_with("/docspec/app/issues/7"),
        "a plan run is about an issue: {}",
        first["target_url"]
    );
    assert_eq!(first["transcript_kept"], false, "no conversation stored");
    f.dashboard
        .app
        .store
        .record_transcript(
            &RunId::parse("r-plan").unwrap(),
            &TranscriptRecord {
                at: String::new(),
                session: "planner".into(),
                model: "deepseek".into(),
                stop: "EndTurn".into(),
                turns: 1,
                bytes: 2,
                body: "{}".into(),
            },
        )
        .await
        .unwrap();
    let kept = get(&f, "/dashboard/api/v1/tool-calls?outcome=problems", &cookie)
        .await
        .json();
    assert_eq!(kept["items"][0]["transcript_kept"], true);
    assert_eq!(kept["items"][1]["transcript_kept"], false, "another run");
    assert_eq!(kept["items"][1]["run_id"], "r-review");
    let error = &problems["items"][3];
    assert_eq!(
        error["call"]["arguments"], "{\"path\":\"<script>.rs\"}",
        "as stored"
    );
    assert!(
        error["target_url"]
            .as_str()
            .unwrap()
            .ends_with("/docspec/app/pull/7"),
        "a review run is about a pull request: {}",
        error["target_url"]
    );

    let refused = get(
        &f,
        "/dashboard/api/v1/tool-calls?outcome=refused_scope&tool=github__get_file",
        &cookie,
    )
    .await
    .json();
    assert_eq!(outcomes(&refused), ["refused_scope"]);
    let checks = get(
        &f,
        "/dashboard/api/v1/tool-calls?session_kind=check",
        &cookie,
    )
    .await
    .json();
    assert_eq!(checks["items"][0]["call"]["session"], "check-1");

    let mut seen = 0;
    let mut uri = "/dashboard/api/v1/tool-calls?limit=3".to_owned();
    loop {
        let page = get(&f, &uri, &cookie).await.json();
        seen += page["items"].as_array().unwrap().len();
        match page["next"].as_str() {
            Some(next) => uri = format!("/dashboard/api/v1/tool-calls?limit=3&cursor={next}"),
            None => break,
        }
    }
    assert_eq!(seen, 8, "every call once");

    for query in ["outcome=meh", "cursor=bm9wZQ"] {
        let bad = get(
            &f,
            &format!("/dashboard/api/v1/tool-calls?{query}"),
            &cookie,
        )
        .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}");
    }
}
