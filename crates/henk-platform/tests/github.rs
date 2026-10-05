//! GitHub App auth and writer against wiremock.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    missing_docs
)]

use std::time::{Duration, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::review::{CommitSha, LaneName, LaneOutcome, LaneResult, ReviewOutcome};
use henk_domain::run::RunId;
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth, GitHubWriter};
use henk_platform::{DiffSide, PlatformWriter as _, ReviewTarget};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const KEY: &str = include_str!("fixtures/test-app-key.pem");

fn target() -> ReviewTarget {
    ReviewTarget {
        repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
        number: 7,
    }
}

fn marker(kind: MarkerKind) -> String {
    Marker {
        run: RunId::parse("r-1").unwrap(),
        model: ModelId::parse("m").unwrap(),
        requested_by: None,
        kind: Some(kind),
    }
    .render()
}

#[test]
fn app_jwt_has_the_documented_shape() {
    let credentials = AppCredentials::from_pem(12345, 678, KEY).unwrap();
    let jwt = credentials
        .jwt(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        .unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(header, json!({"alg": "RS256", "typ": "JWT"}));
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], "12345");
    assert_eq!(claims["iat"], 1_700_000_000 - 60);
    assert_eq!(claims["exp"], 1_700_000_000 + 540);
    assert_eq!(
        URL_SAFE_NO_PAD.decode(parts[2]).unwrap().len(),
        256,
        "2048-bit signature"
    );
}

#[test]
fn garbage_pem_is_rejected() {
    assert!(AppCredentials::from_pem(1, 2, "not a key").is_err());
    assert!(
        AppCredentials::from_pem(1, 2, "-----BEGIN THING-----\nYWJj\n-----END THING-----").is_err()
    );
}

#[tokio::test]
async fn installation_token_is_minted_once_and_reused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/678/access_tokens"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"token": "ghs_abc", "expires_at": "x"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7"))
        .and(header("authorization", "Bearer ghs_abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "title": "Fix", "state": "open", "draft": false,
            "head": {"sha": SHA}, "base": {"ref": "main"}
        })))
        .expect(2)
        .mount(&server)
        .await;

    let api = GitHubApi::new(
        &server.uri(),
        GitHubAuth::app(AppCredentials::from_pem(1, 678, KEY).unwrap()),
    )
    .unwrap();
    let writer = GitHubWriter::new(api, "meneer-henk[bot]");
    let info = writer.pull_request(&target()).await.unwrap();
    assert_eq!(info.head.as_str(), SHA);
    assert_eq!(info.base_ref, "main");
    let _ = writer.pull_request(&target()).await.unwrap();
}

#[tokio::test]
async fn review_lifecycle_posts_check_run_findings_summary_and_folds() {
    let server = MockServer::start().await;
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    let writer = GitHubWriter::new(api, "meneer-henk[bot]");
    let t = target();
    let commit = CommitSha::parse(SHA).unwrap();

    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/check-runs"))
        .and(body_partial_json(
            json!({"name": "Meneer Henk", "head_sha": SHA, "status": "in_progress"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 555})))
        .expect(1)
        .mount(&server)
        .await;
    let handle = writer
        .start_review(&t, &commit, "https://henk/runs/r-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handle.0, "555");

    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .and(body_partial_json(
            json!({"commit_id": SHA, "path": "src/a.rs", "line": 10, "side": "RIGHT"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(
            json!({"id": 1001, "node_id": "PRRC_1", "html_url": "https://gh/c/1001"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let posted = writer
        .post_finding(&t, &commit, "src/a.rs", 10, DiffSide::Right, "Off by one.")
        .await
        .unwrap();
    assert_eq!(posted.id, "1001");
    assert_eq!(posted.node_id.as_deref(), Some("PRRC_1"));

    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1001, "node_id": "PRRC_1", "path": "src/a.rs", "line": 10, "body": format!("Off by one.\n\n{}", marker(MarkerKind::Finding)), "user": {"login": "meneer-henk[bot]"}},
            {"id": 1002, "node_id": "PRRC_2", "path": "src/a.rs", "line": 10, "in_reply_to_id": 1001, "body": "Fixed, thanks", "user": {"login": "alice"}},
            {"id": 1003, "node_id": "PRRC_3", "path": "src/b.rs", "line": null, "body": format!("Old.\n\n{}", marker(MarkerKind::Finding)), "user": {"login": "meneer-henk[bot]"}},
            {"id": 1004, "node_id": "PRRC_4", "path": "src/c.rs", "line": 3, "body": "human comment", "user": {"login": "bob"}}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"number": 7}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                {"id": "PRRT_1", "isResolved": true, "comments": {"nodes": [
                    {"databaseId": 1001, "author": {"login": "meneer-henk", "__typename": "Bot"}},
                    {"databaseId": 1002, "author": {"login": "alice", "__typename": "User"}}
                ]}},
                {"id": "PRRT_3", "isResolved": false, "comments": {"nodes": [
                    {"databaseId": 1003, "author": {"login": "meneer-henk", "__typename": "Bot"}}
                ]}}
            ]
        }}}}})))
        .mount(&server)
        .await;
    let findings = writer.existing_findings(&t).await.unwrap();
    assert_eq!(
        findings.len(),
        2,
        "replies and human comments are not findings"
    );
    assert_eq!(findings[0].comment_id, "1001");
    assert!(findings[0].resolved);
    assert!(findings[0].answered_by_person);
    assert_eq!(findings[0].line, Some(10));
    assert_eq!(findings[1].comment_id, "1003");
    assert_eq!(findings[1].line, None, "outdated");
    assert!(!findings[1].answered_by_person);

    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 2001, "node_id": "IC_1", "body": format!("No issues found.\n\n{}", marker(MarkerKind::Summary)), "user": {"login": "meneer-henk[bot]"}},
            {"id": 2002, "node_id": "IC_2", "body": format!("Hello.\n\n{}", marker(MarkerKind::Reply)), "user": {"login": "meneer-henk[bot]"}},
            {"id": 2003, "node_id": "IC_3", "body": "lgtm", "user": {"login": "alice"}}
        ])))
        .mount(&server)
        .await;
    let summaries = writer.existing_summaries(&t).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].comment_id, "2001");

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"id": "IC_1"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"minimizeComment": {"minimizedComment": {"isMinimized": true}}}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    writer.fold_summary(&t, &summaries[0]).await.unwrap();

    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/issues/7/comments"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"id": 2004, "node_id": "IC_4", "html_url": "u"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let summary = writer.post_comment(&t, "1 issue found.").await.unwrap();
    assert_eq!(summary.id, "2004");

    Mock::given(method("PATCH"))
        .and(path("/repos/docspec/app/check-runs/555"))
        .and(body_partial_json(json!({"status": "completed", "conclusion": "neutral", "output": {"title": "1 issue found."}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 555})))
        .expect(1)
        .mount(&server)
        .await;
    let outcome = ReviewOutcome {
        commit: commit.clone(),
        lanes: vec![LaneResult {
            lane: LaneName::new("a"),
            outcome: LaneOutcome::Finished,
        }],
        open_findings: 1,
    };
    writer
        .finish_review(
            &t,
            &commit,
            Some(&handle),
            &outcome,
            "https://henk/runs/r-1",
        )
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/issues/comments/2003/reactions"))
        .and(body_partial_json(json!({"content": "eyes"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    writer.acknowledge(&t, "2003", false).await.unwrap();
}

#[tokio::test]
async fn api_errors_carry_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_string("{\"message\":\"Not Found\"}"))
        .mount(&server)
        .await;
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    let writer = GitHubWriter::new(api, "x");
    let error = writer.pull_request(&target()).await.unwrap_err();
    assert!(
        matches!(
            error,
            henk_platform::PlatformError::Status { status: 404, .. }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn get_as_app_uses_the_jwt_not_an_installation_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/app/installations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 678, "account": {"login": "docspec", "type": "User"}, "repository_selection": "selected"}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let api = GitHubApi::new(
        &server.uri(),
        GitHubAuth::app(AppCredentials::from_pem(12345, 678, KEY).unwrap()),
    )
    .unwrap();
    let value = api.get_as_app("/app/installations").await.unwrap();
    assert_eq!(value[0]["id"], 678);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "no token was minted");
    let auth = requests[0].headers["authorization"].to_str().unwrap();
    let jwt = auth.strip_prefix("Bearer ").unwrap();
    assert_eq!(
        jwt.split('.').count(),
        3,
        "a JWT, not an installation token"
    );

    let fixed = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    assert!(matches!(
        fixed.get_as_app("/app").await,
        Err(henk_platform::PlatformError::Auth(_))
    ));
}

#[tokio::test]
async fn diff_compares_base_with_the_reviewed_commit_as_a_unified_diff() {
    let server = MockServer::start().await;
    let unified = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n keep\ndiff --git a/new.md b/new.md\nnew file mode 100644\n--- /dev/null\n+++ b/new.md\n@@ -0,0 +1 @@\n+hello\n";
    Mock::given(method("GET"))
        .and(path(format!("/repos/docspec/app/compare/main...{SHA}")))
        .and(header("accept", "application/vnd.github.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(unified))
        .expect(1)
        .mount(&server)
        .await;
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    let writer = GitHubWriter::new(api, "meneer-henk[bot]");
    let commit = CommitSha::parse(SHA).unwrap();
    let patches = writer.diff(&target(), &commit, "main").await.unwrap();
    assert_eq!(patches.len(), 2);
    assert_eq!(patches[0].new_path.as_deref(), Some("src/a.rs"));
    assert!(patches[0].patch.starts_with("@@ -1,2 +1,2 @@"));
    assert_eq!(patches[1].status, henk_domain::diff::FileStatus::Added);
    assert_eq!(patches[1].old_path, None);
}
