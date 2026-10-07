#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use henk_store::{
    DraftDecision, DraftRecord, DraftVerdict, InboundEvent, NewRun, RunStatus, ToolCallRecord,
    TranscriptRecord,
};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::types;
use crate::dashboard::auth::CSRF_HEADER;
use crate::dashboard::session::Session;
use crate::dashboard::tests::{ALLOWED, Fixture, fixture, outcomes_of, seed, signed_in_as};

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
            "no token",
            vec![("cookie", cookie.as_str()), ("origin", ORIGIN)],
            "application/json",
        ),
        (
            "another session's token",
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
        assert_eq!(answer.code(), "forbidden", "{why}");
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
        json!({"github_id": ALLOWED, "login": "alice", "csrf": csrf})
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

/// The SPA's TypeScript types, generated from the API's. `HENK_BLESS=1`
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
        types::RunDetail::decl(&cfg),
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
