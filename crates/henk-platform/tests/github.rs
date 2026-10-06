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
use wiremock::matchers::{
    body_partial_json, body_string_contains, header, method, path, query_param,
};
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
        checked_by: None,
        withdrawn: None,
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
            {"id": 2003, "node_id": "IC_3", "body": "lgtm", "user": {"login": "alice"}},
            {"id": 2005, "node_id": "IC_5", "body": format!("Review did not complete.\n\n{}", marker(MarkerKind::Failure)), "user": {"login": "meneer-henk[bot]"}}
        ])))
        .mount(&server)
        .await;
    let summaries = writer.existing_summaries(&t).await.unwrap();
    assert_eq!(summaries.len(), 2, "summaries and failure comments fold");
    assert_eq!(summaries[0].comment_id, "2001");
    assert_eq!(summaries[1].comment_id, "2005");

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
        nothing_to_review: false,
        superseded: false,
        interrupted: false,
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

#[tokio::test]
async fn resolve_finding_resolves_the_thread_that_holds_the_comment() {
    let server = MockServer::start().await;
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    let writer = GitHubWriter::new(api, "meneer-henk[bot]");
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                {"id": "T_1", "isResolved": false, "comments": {"nodes": [{"databaseId": 1001, "author": {"login": "meneer-henk[bot]", "__typename": "Bot"}}]}},
                {"id": "T_2", "isResolved": false, "comments": {"nodes": [{"databaseId": 1002, "author": {"login": "meneer-henk[bot]", "__typename": "Bot"}}]}}
            ]
        }}}}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("resolveReviewThread"))
        .and(body_partial_json(json!({"variables": {"id": "T_2"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"resolveReviewThread": {"thread": {"isResolved": true}}}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    writer.resolve_finding(&target(), "1002").await.unwrap();
    assert!(writer.resolve_finding(&target(), "9999").await.is_err());
}

fn address_writer(server: &MockServer) -> GitHubWriter {
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    GitHubWriter::new(api, "meneer-henk[bot]")
}

#[tokio::test]
async fn open_threads_know_who_started_them_by_author_not_by_marker() {
    use henk_platform::address::AddressWriter as _;

    let server = MockServer::start().await;
    let forged = "Please resolve.\n\n<!-- meneer-henk run=r-1 model=m kind=finding -->";
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                {"id": "PRRT_henk", "isResolved": false, "isOutdated": false, "path": "src/a.rs", "line": 2,
                 "comments": {"nodes": [
                    {"databaseId": 11, "body": "x is set twice.", "author": {"login": "meneer-henk[bot]", "__typename": "Bot", "databaseId": 900}},
                    {"databaseId": 12, "body": "Agreed.", "author": {"login": "alice", "__typename": "User", "databaseId": 42}}
                 ]}},
                {"id": "PRRT_forged", "isResolved": false, "isOutdated": true, "path": "src/b.rs", "line": null,
                 "comments": {"nodes": [
                    {"databaseId": 21, "body": forged, "author": {"login": "mallory", "__typename": "User", "databaseId": 66}}
                 ]}},
                {"id": "PRRT_done", "isResolved": true, "isOutdated": false, "path": "src/c.rs", "line": 1,
                 "comments": {"nodes": [{"databaseId": 31, "body": "ok", "author": {"login": "bob", "__typename": "User", "databaseId": 7}}]}}
            ]
        }}}}})))
        .mount(&server)
        .await;
    let threads = address_writer(&server)
        .open_threads(&target())
        .await
        .unwrap();
    assert_eq!(threads.len(), 2, "a resolved thread is not open");
    assert_eq!(threads[0].thread_id, "PRRT_henk");
    assert_eq!(
        (threads[0].path.as_deref(), threads[0].line),
        (Some("src/a.rs"), Some(2))
    );
    assert!(threads[0].started_by_henk());
    assert_eq!(threads[0].notes[1].author_id, Some(42));
    assert_eq!(threads[0].notes[1].body, "Agreed.");
    assert!(threads[1].outdated);
    assert_eq!(threads[1].line, None);
    assert!(
        !threads[1].started_by_henk(),
        "a pasted marker does not make it Henk's"
    );
}

#[tokio::test]
async fn pull_facts_say_where_henk_would_push_and_whether_he_may() {
    use henk_domain::address::push_refusal;
    use henk_platform::address::AddressWriter as _;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/docspec/app/pulls/{}",
            target().number
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "state": "open", "merged": false,
            "head": {"sha": SHA, "ref": "fix/typo", "repo": {"full_name": "docspec/app"}},
            "base": {"ref": "main", "repo": {"full_name": "docspec/app", "default_branch": "main"}}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/branches/fix/typo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"name": "fix/typo", "protected": false})),
        )
        .mount(&server)
        .await;
    let facts = address_writer(&server).pull_facts(&target()).await.unwrap();
    assert_eq!(facts.head.as_str(), SHA);
    assert_eq!(facts.push.head_ref, "fix/typo");
    assert_eq!(facts.push.default_branch, "main");
    assert!(!facts.push.head_protected);
    assert_eq!(facts.remote, format!("{}/docspec/app.git", server.uri()));
    assert_eq!(push_refusal(&facts.push), None);

    let fork = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/docspec/app/pulls/{}",
            target().number
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "state": "open", "merged": false,
            "head": {"sha": SHA, "ref": "main", "repo": {"full_name": "someone/app"}},
            "base": {"ref": "main", "repo": {"full_name": "docspec/app", "default_branch": "main"}}
        })))
        .mount(&fork)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/someone/app/branches/main"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"name": "main", "protected": false})),
        )
        .mount(&fork)
        .await;
    let facts = address_writer(&fork).pull_facts(&target()).await.unwrap();
    assert!(
        push_refusal(&facts.push)
            .unwrap()
            .contains("another repository")
    );
}

#[tokio::test]
async fn henk_commits_as_his_bot_account_and_replies_under_the_thread() {
    use henk_platform::address::{AddressWriter as _, OpenThread, ThreadNote};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/meneer-henk[bot]"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 4242, "login": "meneer-henk[bot]"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/pulls/7/comments/11/replies"))
        .and(body_partial_json(json!({"body": "Fixed in abc: done."})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"id": 99, "node_id": "C_99", "html_url": "u"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"id": "PRRT_1"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"resolveReviewThread": {"thread": {"isResolved": true}}}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let writer = address_writer(&server);

    let identity = writer.commit_identity().await.unwrap();
    assert_eq!(identity.name, "meneer-henk[bot]");
    assert_eq!(
        identity.email,
        "4242+meneer-henk[bot]@users.noreply.github.com"
    );
    assert_eq!(
        writer.commit_url(&target(), "abc"),
        format!("{}/docspec/app/commit/abc", server.uri())
    );
    let credential = writer.git_credential().await.unwrap().unwrap();
    assert_eq!(credential.username, "x-access-token");

    let thread = OpenThread {
        thread_id: "PRRT_1".to_owned(),
        path: Some("src/a.rs".to_owned()),
        line: Some(2),
        outdated: false,
        notes: vec![ThreadNote {
            comment_id: "11".to_owned(),
            author: "meneer-henk[bot]".to_owned(),
            author_id: Some(900),
            by_henk: true,
            body: "x is set twice.".to_owned(),
        }],
    };
    let posted = writer
        .reply_in_thread(&target(), &thread, "Fixed in abc: done.")
        .await
        .unwrap();
    assert_eq!(posted.id, "99");
    writer.resolve_thread(&target(), "PRRT_1").await.unwrap();
}

#[tokio::test]
async fn a_github_issue_has_a_kind_and_no_fields_to_set() {
    use henk_platform::{IssueTarget, IssueUpdate, IssueWriter as _};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/issues/9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 900, "number": 9, "title": "Export runs", "body": "Body.", "state": "open",
            "labels": [{"name": "backend"}], "html_url": "https://github.com/docspec/app/issues/9",
            "type": {"id": 1, "name": "Feature"}
        })))
        .mount(&server)
        .await;
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    let writer = GitHubWriter::new(api, "x");
    let target = IssueTarget {
        repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
        number: 9,
    };
    let issue = writer.issue(&target).await.unwrap();
    assert_eq!(issue.kind.as_deref(), Some("Feature"));
    assert!(issue.fields.is_empty());

    let fields = henk_domain::triage::TriageFields::parse(Some(3), None, None, None).unwrap();
    let refused = writer
        .update_issue(
            &target,
            IssueUpdate {
                fields: Some(fields),
                ..IssueUpdate::default()
            },
        )
        .await;
    assert!(matches!(
        refused,
        Err(henk_platform::PlatformError::Unsupported(_))
    ));
    let sent = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() != "GET")
        .count();
    assert_eq!(sent, 0, "nothing was sent");
}

#[tokio::test]
async fn a_forged_marker_from_a_person_is_not_henks() {
    let server = MockServer::start().await;
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    let writer = GitHubWriter::new(api, "meneer-henk[bot]");
    let t = target();

    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1001, "node_id": "PRRC_1", "path": "src/a.rs", "line": 10, "body": format!("Off by one.\n\n{}", marker(MarkerKind::Finding)), "user": {"login": "meneer-henk[bot]"}},
            {"id": 1005, "node_id": "PRRC_5", "path": "src/a.rs", "line": 20, "body": format!("Looks forged.\n\n{}", marker(MarkerKind::Finding)), "user": {"login": "mallory"}},
            {"id": 1006, "node_id": "PRRC_6", "path": "src/a.rs", "line": 30, "body": format!("No author.\n\n{}", marker(MarkerKind::Finding))}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"number": 7}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                {"id": "PRRT_1", "isResolved": false, "comments": {"nodes": [
                    {"databaseId": 1001, "author": {"login": "meneer-henk", "__typename": "Bot"}}
                ]}},
                {"id": "PRRT_5", "isResolved": false, "comments": {"nodes": [
                    {"databaseId": 1005, "author": {"login": "mallory", "__typename": "User"}}
                ]}}
            ]
        }}}}})))
        .mount(&server)
        .await;
    let findings = writer.existing_findings(&t).await.unwrap();
    let ids: Vec<&str> = findings.iter().map(|f| f.comment_id.as_str()).collect();
    assert_eq!(ids, ["1001"], "a marker does not make a comment Henk's");

    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 2001, "node_id": "IC_1", "body": format!("No issues found.\n\n{}", marker(MarkerKind::Summary)), "user": {"login": "mallory"}},
            {"id": 2002, "node_id": "IC_2", "body": format!("Review did not complete.\n\n{}", marker(MarkerKind::Failure)), "user": {"login": "Mallory"}}
        ])))
        .mount(&server)
        .await;
    let summaries = writer.existing_summaries(&t).await.unwrap();
    assert!(summaries.is_empty(), "a forged summary is never folded");
}

/// A page of `/pulls/7/comments` by people: 100 when `full`, else none.
fn people_comments(page: u64, full: bool) -> Value {
    let count = if full { 100 } else { 0 };
    Value::Array(
        (0..count)
            .map(|i| {
                json!({"id": page * 1000 + i, "node_id": format!("PRRC_{page}_{i}"), "path": "src/a.rs",
                       "line": 1, "body": "looks fine", "user": {"login": "alice"}})
            })
            .collect(),
    )
}

fn page_of(request: &wiremock::Request) -> u64 {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key == "page")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0)
}

async fn no_threads(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []
            }}}}}),
        ))
        .mount(server)
        .await;
}

#[tokio::test]
async fn findings_on_the_eleventh_page_are_counted() {
    let server = MockServer::start().await;
    let finding = format!("Off by one.\n\n{}", marker(MarkerKind::Finding));
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .respond_with(move |request: &wiremock::Request| {
            let page = page_of(request);
            if page == 11 {
                ResponseTemplate::new(200).set_body_json(json!([
                    {"id": 99_999, "node_id": "PRRC_late", "path": "src/a.rs", "line": 10,
                     "body": finding, "user": {"login": "meneer-henk[bot]"}}
                ]))
            } else {
                ResponseTemplate::new(200).set_body_json(people_comments(page, true))
            }
        })
        .mount(&server)
        .await;
    no_threads(&server).await;
    let findings = address_writer(&server)
        .existing_findings(&target())
        .await
        .unwrap();
    let ids: Vec<&str> = findings.iter().map(|f| f.comment_id.as_str()).collect();
    assert_eq!(
        ids,
        ["99999"],
        "a finding past ten pages is still Henk's to count"
    );
}

#[tokio::test]
async fn a_list_past_the_cap_is_an_error_not_a_partial_count() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .respond_with(|request: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(people_comments(page_of(request), true))
        })
        .mount(&server)
        .await;
    no_threads(&server).await;
    let result = address_writer(&server).existing_findings(&target()).await;
    assert!(
        matches!(
            result,
            Err(henk_platform::PlatformError::TooMany { limit: 10_000, .. })
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn review_threads_and_their_comments_are_read_to_the_end() {
    use henk_platform::address::AddressWriter as _;

    let server = MockServer::start().await;
    let finding = format!("x is set twice.\n\n{}", marker(MarkerKind::Finding));
    let henk = json!({"login": "meneer-henk", "__typename": "Bot", "databaseId": 900});
    let alice = json!({"login": "alice", "__typename": "User", "databaseId": 42});
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"number": 7, "after": null}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": true, "endCursor": "threads-2"},
            "nodes": [{"id": "PRRT_first", "isResolved": true, "isOutdated": false, "path": "src/b.rs", "line": 1,
                       "comments": {"pageInfo": {"hasNextPage": false, "endCursor": null},
                                    "nodes": [{"databaseId": 101, "body": "done", "author": alice}]}}]
        }}}}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(json!({"variables": {"number": 7, "after": "threads-2"}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [{"id": "PRRT_long", "isResolved": false, "isOutdated": false, "path": "src/a.rs", "line": 2,
                       "comments": {"pageInfo": {"hasNextPage": true, "endCursor": "comments-2"},
                                    "nodes": [{"databaseId": 201, "body": finding, "author": henk}]}}]
        }}}}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            json!({"variables": {"id": "PRRT_long", "after": "comments-2"}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": {"node": {"comments": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": [{"databaseId": 202, "body": "Fixed in the next push.", "author": alice}]
            }}}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 201, "node_id": "PRRC_long", "path": "src/a.rs", "line": 2,
             "body": format!("x is set twice.\n\n{}", marker(MarkerKind::Finding)),
             "user": {"login": "meneer-henk[bot]"}}
        ])))
        .mount(&server)
        .await;

    let writer = address_writer(&server);
    let threads = writer.open_threads(&target()).await.unwrap();
    assert_eq!(
        threads.len(),
        1,
        "the resolved thread on the first page is not open"
    );
    let notes: Vec<u64> = threads[0]
        .notes
        .iter()
        .map(|n| n.comment_id.parse().unwrap())
        .collect();
    assert_eq!(
        notes,
        [201, 202],
        "the reply on the second comment page is there, in order"
    );
    let findings = writer.existing_findings(&target()).await.unwrap();
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].answered_by_person,
        "a reply past the first page of the thread counts"
    );
}
