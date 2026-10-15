#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::run::{RunId, RunKind};
use henk_mcp::{McpServerConfig, McpSession as _, RmcpSession};
use henk_store::{DraftRecord, FindingAction, NewRun, RoundRecord, SessionRole, TranscriptRecord};
use secrecy::SecretString;
use serde_json::{Value, json};

use super::Token;
use crate::app::App;
use crate::config::{Config, McpScope};
use crate::listeners::testing::FakeWriter;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const WRITE: &str = "w-token-0123456789";
const READ: &str = "r-token-0123456789";

fn config(mcp_server: &str) -> String {
    format!(
        r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
{mcp_server}
"#
    )
}

const ON: &str = "[mcp_server]\nenabled = true\n[[mcp_server.tokens]]\nname = \"claude\"\nenv = \"UNUSED_W\"\nscope = \"write\"\n[[mcp_server.tokens]]\nname = \"reader\"\nenv = \"UNUSED_R\"\nscope = \"read\"\n";

struct Henk {
    app: Arc<App>,
    coordinator: Arc<crate::coordinator::Coordinator>,
    url: String,
    _server: tokio::task::JoinHandle<()>,
}

/// Henk with its listeners, a fake platform and `/mcp` on a local port,
/// with `tokens` (none: the configured ones are not given).
async fn start_henk(mcp_server: &str, tokens: Vec<Token>) -> Henk {
    // The clients' TLS stack wants its provider, as Henk's own clients do.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let settings = Config::parse(&config(mcp_server))
        .and_then(Config::into_settings)
        .unwrap_or_else(|e| panic!("{e}"));
    let writer = Arc::new(FakeWriter {
        head: SHA.to_owned(),
        ..FakeWriter::default()
    });
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
        own_pushes: crate::push::OwnPushes::default(),
        workspace_provider: Arc::new(crate::workspace::host::HostProvider),
        test_writer: Some(writer),
        test_session: None,
        test_push_writer: None,
        test_issue_writer: None,
        test_ci_reader: None,
    });
    let composed = crate::server::compose_with_secrets(&app, None, None, None);
    let router = super::routes(
        Arc::clone(&app),
        Arc::clone(&composed.coordinator),
        Arc::clone(&composed.bus),
        composed.bus.listeners().collect(),
        tokens,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Henk {
        app,
        coordinator: Arc::clone(&composed.coordinator),
        url: format!("http://127.0.0.1:{}/mcp", address.port()),
        _server: server,
    }
}

fn both() -> Vec<Token> {
    vec![
        Token::new(
            "claude",
            McpScope::Write,
            SecretString::from(WRITE.to_owned()),
        ),
        Token::new(
            "reader",
            McpScope::Read,
            SecretString::from(READ.to_owned()),
        ),
    ]
}

/// Henk's own MCP client, with `token`.
async fn client(henk: &Henk, token: &str) -> RmcpSession {
    let config: McpServerConfig =
        toml::from_str(&format!("url = \"{}\"\nbearer_env = \"T\"\n", henk.url)).unwrap();
    let token = token.to_owned();
    RmcpSession::connect("henk", &config, move |name| {
        (name == "T").then(|| token.clone())
    })
    .await
    .unwrap()
}

async fn call(session: &RmcpSession, tool: &str, arguments: Value) -> (bool, Value, String) {
    let outcome = session.call_tool(tool, arguments).await.unwrap();
    (
        outcome.is_error,
        outcome.structured.unwrap_or(Value::Null),
        outcome.text,
    )
}

async fn seed_run(app: &App, id: &str) -> RunId {
    let run = RunId::parse(id).unwrap();
    app.store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Review,
            platform: Platform::GitHub,
            repo: "docspec/app".into(),
            target: 7,
            commit: Some(SHA.into()),
            requester: None,
            trigger: "opened".into(),
            link: String::new(),
        })
        .await
        .unwrap();
    run
}

#[tokio::test]
async fn a_client_lists_the_tools_and_reads_runs_events_and_health() {
    let henk = start_henk(ON, both()).await;
    let run = seed_run(&henk.app, "r-1").await;
    henk.app
        .store
        .event(&run, "info", "diff: 3 files")
        .await
        .unwrap();
    henk.app
        .store
        .start_round(
            &run,
            &RoundRecord {
                session: "reviewer".into(),
                role: SessionRole::Reviewer,
                round: 1,
                started_at: String::new(),
                ended_at: None,
                first_turn: 1,
                turns: 0,
                handover: "Review this.".into(),
                handover_len: 12,
            },
        )
        .await
        .unwrap();
    let session = client(&henk, READ).await;

    let mut names: Vec<String> = session
        .list_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "cancel_run",
            "get_event",
            "get_run",
            "get_run_events",
            "get_transcript",
            "health",
            "list_queued_runs",
            "list_runs",
            "list_tool_calls",
            "move_queued_run",
            "start_execute",
            "start_plan",
            "start_rebase",
            "start_review",
        ]
    );

    let (error, listed, _) = call(&session, "list_runs", json!({ "repo": "docspec/app" })).await;
    assert!(!error);
    assert_eq!(listed["items"][0]["id"], "r-1");
    let (_, bad, text) = call(&session, "list_runs", json!({ "kind": "robot" })).await;
    assert_eq!(bad, Value::Null);
    assert!(text.starts_with("kind is one of review, plan"), "{text}");

    let (error, detail, _) = call(&session, "get_run", json!({ "run_id": "r-1" })).await;
    assert!(!error);
    assert_eq!(detail["run"]["repo"], "docspec/app");
    assert_eq!(
        detail["rounds"][0]["role"], "reviewer",
        "a pairing's rounds (#374)"
    );
    assert_eq!(detail["rounds"][0]["handover"], "Review this.");
    let (error, _, text) = call(&session, "get_run", json!({ "run_id": "r-404" })).await;
    assert!(error);
    assert_eq!(text, "No such run.");

    let (_, events, _) = call(&session, "get_run_events", json!({ "run_id": "r-1" })).await;
    assert_eq!(events["items"][0]["message"], "diff: 3 files");
    let (_, calls, _) = call(&session, "list_tool_calls", json!({ "run_id": "r-1" })).await;
    assert_eq!(calls["items"], json!([]));
    let (error, health, _) = call(&session, "health", json!({})).await;
    assert!(!error);
    assert!(
        health["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "database")
    );
}

/// `get_run` speaks of sessions and their roles (#325), also for a review
/// from before the loop: its lanes and fact-check are sessions with their
/// history roles, and its drafts and findings name their session.
#[tokio::test]
async fn get_run_gives_a_lane_era_run_s_sessions_with_their_roles() {
    let henk = start_henk(ON, both()).await;
    let run = seed_run(&henk.app, "r-lanes").await;
    let store = &henk.app.store;
    for (name, role) in [
        ("lane-a", SessionRole::Lane),
        ("check-1", SessionRole::Check),
    ] {
        store.start_session(&run, name, role, "m").await.unwrap();
    }
    store
        .record_draft(
            &run,
            &DraftRecord {
                at: String::new(),
                draft: "d1".into(),
                session: "lane-a".into(),
                model: "m".into(),
                kind: "finding".into(),
                path: "a.rs".into(),
                line: 4,
                target: String::new(),
                body: "x is never set.".into(),
                decision: None,
            },
        )
        .await
        .unwrap();
    store
        .record_finding(&run, "lane-a", "a.rs", 4, "c-1", FindingAction::Posted)
        .await
        .unwrap();
    let session = client(&henk, READ).await;

    let (error, detail, text) = call(&session, "get_run", json!({ "run_id": "r-lanes" })).await;
    assert!(!error, "{text}");
    let sessions: Vec<(&str, &str)> = detail["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap(), s["role"].as_str().unwrap()))
        .collect();
    assert_eq!(sessions, [("check-1", "check"), ("lane-a", "lane")]);
    assert!(detail.get("lanes").is_none(), "{detail}");
    assert_eq!(detail["rounds"], json!([]), "a lane-era run has no rounds");
    assert_eq!(detail["drafts"][0]["session"], "lane-a");
    assert_eq!(detail["findings"][0]["session"], "lane-a");
}

#[tokio::test]
async fn a_write_token_starts_a_review_as_its_own_requester_and_a_read_token_may_not() {
    let henk = start_henk(ON, both()).await;
    let writer = client(&henk, WRITE).await;
    let url = "https://github.com/docspec/app/pull/7";
    let (error, started, text) = call(&writer, "start_review", json!({ "url": url })).await;
    assert!(!error, "{text}");
    assert_eq!(started["outcome"], "started", "{started}");
    let run = started["run_id"].as_str().unwrap().to_owned();
    assert!(
        started["run_link"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/runs/{run}"))
    );

    let (_, event, _) = call(
        &writer,
        "get_event",
        json!({ "event_id": started["event_id"] }),
    )
    .await;
    assert_eq!(event["event"]["source"], "mcp");
    assert_eq!(event["event"]["requester"], "mcp:claude");

    let (error, _, text) = call(
        &writer,
        "start_review",
        json!({ "url": "https://github.com/elsewhere/app/pull/1" }),
    )
    .await;
    assert!(error, "refused by the allowlist");
    assert!(
        text.starts_with("Not started: ") && text.contains("allowlist"),
        "{text}"
    );
    let (error, _, text) = call(&writer, "start_review", json!({ "url": "not a url" })).await;
    assert!(error, "{text}");

    let reader = client(&henk, READ).await;
    let (error, _, text) = call(&reader, "start_review", json!({ "url": url })).await;
    assert!(error);
    assert_eq!(
        text,
        "This token may only read: starting, cancelling and moving need a write token."
    );
    henk.app.shutdown.cancel();
}

/// #329: `start_execute` goes through the execute listener, which says why
/// nothing started when this Henk takes no issues to pull requests.
#[tokio::test]
async fn start_execute_answers_with_the_execute_listeners_outcome() {
    let henk = start_henk(ON, both()).await;
    let writer = client(&henk, WRITE).await;
    let url = "https://github.com/docspec/app/issues/9";
    let (error, _, text) = call(&writer, "start_execute", json!({ "url": url })).await;
    assert!(error, "{text}");
    assert_eq!(text, "Not started: execution is not configured.");
    let (error, _, text) = call(
        &writer,
        "start_execute",
        json!({ "url": "https://github.com/docspec/app/pull/9" }),
    )
    .await;
    assert!(error, "a pull request is not an issue: {text}");

    let reader = client(&henk, READ).await;
    let (error, _, text) = call(&reader, "start_execute", json!({ "url": url })).await;
    assert!(error);
    assert_eq!(
        text,
        "This token may only read: starting, cancelling and moving need a write token."
    );
    henk.app.shutdown.cancel();
}

#[tokio::test]
async fn a_read_token_may_not_cancel_a_running_run() {
    let henk = start_henk(ON, both()).await;
    let run = seed_run(&henk.app, "r-4").await;
    let token = tokio_util::sync::CancellationToken::new();
    let _cancellable = henk.app.cancels.register(run, token.clone());
    let reader = client(&henk, READ).await;
    let (error, _, text) = call(&reader, "cancel_run", json!({ "run_id": "r-4" })).await;
    assert!(error, "a read token may not cancel");
    assert_eq!(
        text,
        "This token may only read: starting, cancelling and moving need a write token."
    );
    assert!(!token.is_cancelled(), "the run is still running");
}

#[tokio::test]
async fn a_write_token_cancels_a_running_run_and_the_cancel_is_on_record() {
    let henk = start_henk(ON, both()).await;
    let run = seed_run(&henk.app, "r-2").await;
    let token = tokio_util::sync::CancellationToken::new();
    let cancellable = henk.app.cancels.register(run.clone(), token.clone());
    let session = client(&henk, WRITE).await;
    let (error, cancelled, text) = call(&session, "cancel_run", json!({ "run_id": "r-2" })).await;
    assert!(!error, "{text}");
    assert_eq!(cancelled["run_id"], "r-2");
    assert!(token.is_cancelled());
    // The run ends, as a cancelled run does, and is no longer cancellable.
    drop(cancellable);
    let (error, _, text) = call(&session, "cancel_run", json!({ "run_id": "r-2" })).await;
    assert!(error, "not running any more");
    assert!(text.starts_with("That run is not running here"), "{text}");
}

#[tokio::test]
async fn a_write_token_moves_a_waiting_review_that_list_queued_runs_shows() {
    let henk = start_henk(ON, both()).await;
    // Every slot taken, so the reviews wait.
    let held: Vec<_> = std::iter::from_fn(|| henk.coordinator.take_a_slot()).collect();
    assert!(!held.is_empty());
    let commit = henk_domain::review::CommitSha::parse(SHA).unwrap();
    let mut runs = Vec::new();
    for number in [8, 9] {
        let request = crate::review::ReviewRequest {
            target: henk_platform::ReviewTarget {
                repo: henk_domain::allowlist::RepoRef::parse(
                    henk_domain::allowlist::Platform::GitHub,
                    "docspec/app",
                )
                .unwrap(),
                number,
            },
            commit: None,
            trigger: "test".to_owned(),
            requester: None,
            acknowledge: None,
            run: None,
            submitted_at: None,
            queued_check: None,
        };
        runs.push(henk.coordinator.submit_review(request, commit.clone()).1);
    }
    let reader = client(&henk, READ).await;
    let (error, queue, text) = call(&reader, "list_queued_runs", json!({})).await;
    assert!(!error, "{text}");
    let order = |queue: &Value| {
        queue["waiting"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["run_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&queue), [runs[0].as_str(), runs[1].as_str()]);
    let (error, _, text) = call(
        &reader,
        "move_queued_run",
        json!({ "run_id": runs[1].as_str(), "position": 1 }),
    )
    .await;
    assert!(error);
    assert_eq!(
        text,
        "This token may only read: starting, cancelling and moving need a write token."
    );

    let writer = client(&henk, WRITE).await;
    let (error, moved, text) = call(
        &writer,
        "move_queued_run",
        json!({ "run_id": runs[1].as_str(), "position": 1 }),
    )
    .await;
    assert!(!error, "{text}");
    assert_eq!(
        (moved["from"].as_u64(), moved["to"].as_u64()),
        (Some(2), Some(1))
    );
    let (_, queue, _) = call(&writer, "list_queued_runs", json!({})).await;
    assert_eq!(order(&queue), [runs[1].as_str(), runs[0].as_str()]);
    let recorded = henk
        .app
        .store
        .inbound_events_for_run(&runs[1])
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "queue_moved")
        .map(|e| e.requester)
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        [Some("mcp:claude".to_owned())],
        "the write token's name"
    );
    henk.app.shutdown.cancel();
}

#[tokio::test]
async fn a_long_transcript_comes_a_page_at_a_time_under_its_size() {
    let henk = start_henk(ON, both()).await;
    let run = seed_run(&henk.app, "r-3").await;
    let big = "x".repeat(100 * 1024);
    let messages: Vec<Value> = (0..6)
        .map(|_| json!({ "role": "assistant", "blocks": [{ "text": big }] }))
        .collect();
    let body = json!({ "session": "lane-a", "model": "m", "stop": "EndTurn", "turns": 6, "system": "s", "messages": messages });
    henk.app
        .store
        .record_transcript(
            &run,
            &TranscriptRecord {
                at: String::new(),
                session: "lane-a".into(),
                model: "m".into(),
                stop: "EndTurn".into(),
                turns: 6,
                bytes: 0,
                body: body.to_string(),
            },
        )
        .await
        .unwrap();
    let session = client(&henk, READ).await;
    let (error, page, text) = call(
        &session,
        "get_transcript",
        json!({ "run_id": "r-3", "session": "lane-a" }),
    )
    .await;
    assert!(!error, "{text}");
    assert_eq!(page["cut_for_size"], true);
    let shown = page["transcript"]["messages"].as_array().unwrap().len();
    assert!((1..6).contains(&shown), "{shown}");
    assert_eq!(page["next_from_message"], shown);
}

/// A plain POST to `/mcp`, for what happens before MCP.
async fn post(url: &str, token: Option<&str>, host: Option<&str>) -> reqwest::StatusCode {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(host) = host {
        request = request.header("host", host);
    }
    request.send().await.unwrap().status()
}

#[tokio::test]
async fn no_mcp_without_a_good_token_and_none_when_it_is_off() {
    let henk = start_henk(ON, both()).await;
    assert_eq!(post(&henk.url, None, None).await, 401);
    assert_eq!(post(&henk.url, Some("nope"), None).await, 401);
    assert_ne!(post(&henk.url, Some(READ), None).await, 401);
    assert!(
        post(&henk.url, Some(READ), Some("evil.example"))
            .await
            .is_client_error(),
        "another host is refused"
    );

    let no_tokens = henk_plain(ON).await;
    assert_eq!(post(&no_tokens, Some(READ), None).await, 503);
    let off = start_henk("[mcp_server]\nenabled = false\n", both()).await;
    assert_eq!(post(&off.url, Some(READ), None).await, 503);
    let absent = start_henk("", both()).await;
    assert_eq!(post(&absent.url, Some(READ), None).await, 503);
}

#[tokio::test]
async fn a_secret_two_tokens_share_is_refused_rather_than_given_a_scope() {
    let mut tokens = both();
    tokens.push(Token::new(
        "also-reader",
        McpScope::Read,
        SecretString::from(WRITE.to_owned()),
    ));
    let henk = start_henk(ON, tokens).await;
    assert_eq!(post(&henk.url, Some(WRITE), None).await, 401);
    assert_ne!(post(&henk.url, Some(READ), None).await, 401);
}

/// `/mcp` on for config `mcp_server` but with no token given.
async fn henk_plain(mcp_server: &str) -> String {
    start_henk(mcp_server, Vec::new()).await.url
}

#[tokio::test]
async fn doctor_says_which_tokens_are_set_and_probe_lists_the_tools() {
    let henk = start_henk(ON, both()).await;
    let mut settings = henk.app.settings.clone();
    settings.server.public_base_url = henk.url.trim_end_matches("/mcp").to_owned();
    let only_write = |name: &str| (name == "UNUSED_W").then(|| WRITE.to_owned());
    let checks = crate::doctor::check_mcp_server(&settings, true, only_write).await;
    let said: Vec<(String, bool)> = checks
        .iter()
        .map(|c| (format!("{}: {:?}", c.name, c.verdict), c.is_failure()))
        .collect();
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(
        said[0].0.contains("secret $UNUSED_W")
            && said[0].0.contains("set; MCP client claude (write)")
    );
    assert!(
        said[1]
            .0
            .contains("not set; MCP client reader cannot connect")
    );
    assert!(
        said[2].0.contains("14 tools at http://127.0.0.1:"),
        "{said:?}"
    );
    assert!(said[2].0.ends_with("as claude\")"), "{said:?}");
    assert!(said.iter().all(|(_, failed)| !failed));

    let none = crate::doctor::check_mcp_server(&settings, false, |_| None).await;
    assert!(
        none.last().unwrap().is_failure(),
        "on with no token set fails"
    );
    let off = Config::parse(&config("[mcp_server]\nenabled = false\n"))
        .and_then(Config::into_settings)
        .unwrap();
    let checks = crate::doctor::check_mcp_server(&off, true, |_| None).await;
    assert_eq!(checks.len(), 1);
    assert!(!checks[0].is_failure());
}
