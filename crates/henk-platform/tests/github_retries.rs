//! GitHub failing for a moment (#338): reads and updates are tried again,
//! and a create that may have landed is looked up before it is tried again.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::sync::Arc;
use std::time::Duration;

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::review::CommitSha;
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth, GitHubWriter, Installations};
use henk_platform::push::{NewPull, PushWriter as _};
use henk_platform::retry::RetryPolicy;
use henk_platform::{PlatformError, PlatformWriter as _, ReviewTarget};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const BOT: &str = "meneer-henk[bot]";

fn writer(server: &MockServer) -> GitHubWriter {
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into()))
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        });
    GitHubWriter::new(api, BOT)
}

fn target() -> ReviewTarget {
    ReviewTarget {
        repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
        number: 7,
    }
}

async fn count(server: &MockServer, verb: &str, at: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == verb && r.url.path() == at)
        .count()
}

#[tokio::test]
async fn a_read_that_fails_for_a_moment_is_tried_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7"))
        .respond_with(ResponseTemplate::new(502).set_body_string("Bad Gateway"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "title": "Fix", "state": "open", "draft": false,
            "head": {"sha": SHA}, "base": {"ref": "main"}
        })))
        .mount(&server)
        .await;
    let info = writer(&server).pull_request(&target()).await.unwrap();
    assert_eq!(info.base_ref, "main");
    assert_eq!(count(&server, "GET", "/repos/docspec/app/pulls/7").await, 2);
}

#[tokio::test]
async fn a_rate_limit_waits_and_a_refusal_is_not_tried_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    let w = writer(&server);
    assert_eq!(w.api().get("/repos/docspec/app").await.unwrap()["ok"], true);

    for status in [404_u16, 422, 403] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        let error = writer(&server).api().get("/x").await.unwrap_err();
        assert!(matches!(error, PlatformError::Status { status: s, .. } if s == status));
    }
}

#[tokio::test]
async fn a_read_gives_up_with_the_last_error_when_the_attempts_run_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    let error = writer(&server).api().get("/x").await.unwrap_err();
    assert!(matches!(error, PlatformError::Status { status: 503, .. }));
}

#[tokio::test]
async fn a_comment_that_landed_before_github_failed_is_not_posted_again() {
    let server = MockServer::start().await;
    let comments = "/repos/docspec/app/issues/7/comments";
    Mock::given(method("POST"))
        .and(path(comments))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(comments))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "body": "someone else's", "user": {"login": "alice"}},
            {"id": 2, "body": "the summary <!-- meneer-henk run=r-1 -->", "user": {"login": "alice"}},
            {"id": 3, "body": "the summary <!-- meneer-henk run=r-1 -->", "user": {"login": BOT},
             "html_url": "https://github.com/docspec/app/pull/7#issuecomment-3"},
        ])))
        .mount(&server)
        .await;
    let posted = writer(&server)
        .post_comment(&target(), "the summary <!-- meneer-henk run=r-1 -->")
        .await
        .unwrap();
    assert_eq!(posted.id, "3", "Henk's own, not a copy someone else made");
    assert_eq!(count(&server, "POST", comments).await, 1, "posted once");
}

#[tokio::test]
async fn a_comment_that_did_not_land_is_posted_again() {
    let server = MockServer::start().await;
    let comments = "/repos/docspec/app/issues/7/comments";
    Mock::given(method("POST"))
        .and(path(comments))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(comments))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 9, "html_url": "u"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(comments))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let posted = writer(&server)
        .post_comment(&target(), "the summary")
        .await
        .unwrap();
    assert_eq!(posted.id, "9");
    assert_eq!(count(&server, "POST", comments).await, 2);
}

#[tokio::test]
async fn a_reply_that_landed_is_found_among_the_replies_to_its_comment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/pulls/7/comments/41/replies"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 50, "body": "On it.", "user": {"login": BOT}, "in_reply_to_id": 40},
            {"id": 51, "body": "On it.", "user": {"login": BOT}, "in_reply_to_id": 41},
        ])))
        .mount(&server)
        .await;
    let posted = writer(&server)
        .reply(&target(), "41", true, "On it.")
        .await
        .unwrap();
    assert_eq!(posted.id, "51", "the reply to this comment, not another");
}

#[tokio::test]
async fn a_check_run_that_landed_is_found_by_its_run_link() {
    let server = MockServer::start().await;
    let checks = "/repos/docspec/app/check-runs";
    Mock::given(method("POST"))
        .and(path(checks))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/docspec/app/commits/{SHA}/check-runs")))
        .and(query_param("check_name", "Meneer Henk"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"check_runs": [
                {"id": 70, "external_id": "https://henk/runs/r-0"},
                {"id": 71, "external_id": "https://henk/runs/r-1"},
            ]})),
        )
        .mount(&server)
        .await;
    let handle = writer(&server)
        .start_review(
            &target(),
            &CommitSha::parse(SHA).unwrap(),
            "https://henk/runs/r-1",
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handle.0, "71");
    assert_eq!(count(&server, "POST", checks).await, 1);
}

#[tokio::test]
async fn a_pull_request_that_was_opened_is_not_opened_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/pulls"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls"))
        .and(query_param("head", "docspec:henk/issue-3"))
        .and(query_param("base", "main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"number": 12, "html_url": "https://github.com/docspec/app/pull/12"}
        ])))
        .mount(&server)
        .await;
    let opened = writer(&server)
        .open_pull_request(
            &RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
            &NewPull {
                title: "Fix #3".to_owned(),
                head: "henk/issue-3".to_owned(),
                base: "main".to_owned(),
                body: "Closes #3".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(opened.number, 12);
}

#[tokio::test]
async fn a_branch_with_reserved_characters_is_found_when_the_create_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/pulls"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls"))
        .and(query_param("head", "docspec:feat/a+b&c%d e"))
        .and(query_param("base", "release/1.0+x"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"number": 13, "html_url": "https://github.com/docspec/app/pull/13"}
        ])))
        .mount(&server)
        .await;
    let opened = writer(&server)
        .open_pull_request(
            &RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
            &NewPull {
                title: "Fix".to_owned(),
                head: "feat/a+b&c%d e".to_owned(),
                base: "release/1.0+x".to_owned(),
                body: String::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(opened.number, 13);
}

#[tokio::test]
async fn the_clients_retry_policy_covers_the_installation_token() {
    let server = MockServer::start().await;
    let tokens = "/app/installations/222/access_tokens";
    Mock::given(method("POST"))
        .and(path(tokens))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(tokens))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(
                json!({"token": "ghs_docspec", "expires_at": "2999-01-01T00:00:00Z"}),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;
    let auth = GitHubAuth::app(Arc::new(Installations::new(
        AppCredentials::from_pem(1, include_str!("fixtures/test-app-key.pem")).unwrap(),
        None,
    )));
    let api = GitHubApi::new(&server.uri(), auth)
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        })
        .for_installation(222);
    assert_eq!(api.get("/repos/docspec/app").await.unwrap()["ok"], true);
    assert_eq!(count(&server, "POST", tokens).await, 2);
}

fn app_api(server: &MockServer, retry: RetryPolicy) -> GitHubApi {
    let auth = GitHubAuth::app(Arc::new(Installations::new(
        AppCredentials::from_pem(1, include_str!("fixtures/test-app-key.pem")).unwrap(),
        None,
    )));
    GitHubApi::new(&server.uri(), auth)
        .unwrap()
        .with_retry_policy(retry)
}

const FAST: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    base_delay: Duration::from_millis(1),
    max_delay: Duration::from_millis(5),
};

#[tokio::test]
async fn a_failing_token_mint_is_tried_as_often_as_the_policy_says_and_no_more() {
    let server = MockServer::start().await;
    let tokens = "/app/installations/222/access_tokens";
    Mock::given(method("POST"))
        .and(path(tokens))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let error = app_api(&server, FAST)
        .for_installation(222)
        .get("/repos/docspec/app")
        .await
        .unwrap_err();
    assert!(matches!(error, PlatformError::Auth(_)), "{error}");
    assert_eq!(count(&server, "POST", tokens).await, 3);
    assert_eq!(count(&server, "GET", "/repos/docspec/app").await, 0);
}

#[tokio::test]
async fn a_failing_installation_lookup_is_tried_as_often_as_the_policy_says_and_no_more() {
    let server = MockServer::start().await;
    let lookup = "/repos/docspec/app/installation";
    Mock::given(method("GET"))
        .and(path(lookup))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let error = app_api(&server, FAST)
        .for_repository(&target().repo)
        .get("/repos/docspec/app")
        .await
        .unwrap_err();
    assert!(matches!(error, PlatformError::Auth(_)), "{error}");
    assert_eq!(count(&server, "GET", lookup).await, 3);
    assert_eq!(count(&server, "GET", "/repos/docspec/app").await, 0);
}

#[tokio::test]
async fn a_mint_waiting_to_try_again_holds_up_no_other_installation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/222/access_tokens"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/333/access_tokens"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"token": "ghs_alice", "expires_at": "2999-01-01T00:00:00Z"})),
        )
        .mount(&server)
        .await;
    let api = app_api(
        &server,
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(5),
        },
    );
    let failing = api.for_installation(222);
    let backing_off = tokio::spawn(async move { failing.git_token().await });
    // Until the first mint for 222 has failed and its retry is waiting.
    while count(&server, "POST", "/app/installations/222/access_tokens").await == 0 {
        tokio::task::yield_now().await;
    }
    let other = tokio::time::timeout(
        Duration::from_secs(2),
        api.for_installation(333).git_token(),
    )
    .await;
    backing_off.abort();
    let token = other.expect("333 waited for 222's backoff").unwrap();
    assert_eq!(secrecy::ExposeSecret::expose_secret(&token), "ghs_alice");
}

#[tokio::test]
async fn a_connection_that_drops_without_an_answer_is_tried_again() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // First connection: read the request, close without a response.
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 4096];
        let _ = socket.read(&mut buffer).await;
        drop(socket);
        // Second connection: answer.
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = socket.read(&mut buffer).await;
        let body = r#"{"ok":true}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        let _ = socket.shutdown().await;
    });
    let api = GitHubApi::new(
        &format!("http://{address}"),
        GitHubAuth::token("t".to_owned().into()),
    )
    .unwrap()
    .with_retry_policy(FAST);
    assert_eq!(api.get("/x").await.unwrap()["ok"], true);
}
