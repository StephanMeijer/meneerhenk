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
use henk_domain::review::{CommitSha, ReviewOutcome, SessionName, SessionOutcome, SessionResult};
use henk_domain::run::RunId;
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth, GitHubWriter};
use henk_platform::{PlatformWriter as _, ReviewTarget};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
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
async fn review_lifecycle_opens_the_check_posts_a_comment_and_closes_it() {
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
        .start_review(&t, &commit, "https://henk/runs/r-1", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(handle.0, "555");

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
        sessions: vec![SessionResult {
            session: SessionName::new("a"),
            outcome: SessionOutcome::Finished,
        }],
        open_findings: 1,
        nothing_to_review: false,
        stopped: None,
        not_reviewed: None,
        findings_left: false,
        report: None,
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

fn push_writer(server: &MockServer) -> GitHubWriter {
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    GitHubWriter::new(api, "meneer-henk[bot]")
}

#[tokio::test]
async fn pull_facts_say_where_henk_would_push_and_whether_he_may() {
    use henk_domain::push::push_refusal;
    use henk_platform::push::PushWriter as _;

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
    let facts = push_writer(&server).pull_facts(&target()).await.unwrap();
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
    let facts = push_writer(&fork).pull_facts(&target()).await.unwrap();
    assert!(
        push_refusal(&facts.push)
            .unwrap()
            .contains("another repository")
    );
}

#[tokio::test]
async fn a_planner_fetches_the_default_branch_at_its_head() {
    use henk_platform::push::PushWriter as _;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"default_branch": "trunk"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/branches/trunk"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"name": "trunk", "commit": {"sha": SHA}})),
        )
        .mount(&server)
        .await;
    let head = push_writer(&server)
        .repo_head(&target().repo)
        .await
        .unwrap();
    assert_eq!(head.default_branch, "trunk");
    assert_eq!(head.head.as_str(), SHA);
    assert_eq!(head.remote, format!("{}/docspec/app.git", server.uri()));
}

#[tokio::test]
async fn an_executor_opens_a_ready_pull_request_for_its_branch() {
    use henk_platform::push::{NewPull, PushWriter as _};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/pulls"))
        .and(body_partial_json(json!({
            "title": "Export runs", "head": "henk/9-export-runs", "base": "main",
            "body": "Closes #9", "draft": false
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": 12, "html_url": "https://github.com/docspec/app/pull/12"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let opened = push_writer(&server)
        .open_pull_request(
            &target().repo,
            &NewPull {
                head: "henk/9-export-runs".to_owned(),
                base: "main".to_owned(),
                title: "Export runs".to_owned(),
                body: "Closes #9".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(opened.number, 12);
    assert_eq!(opened.url, "https://github.com/docspec/app/pull/12");
}

#[tokio::test]
async fn henk_commits_as_his_bot_account() {
    use henk_platform::push::PushWriter as _;

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/meneer-henk[bot]"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 4242, "login": "meneer-henk[bot]"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 77, "login": "alice"})))
        .mount(&server)
        .await;
    let writer = push_writer(&server);

    let identity = writer.commit_identity().await.unwrap();
    assert_eq!(identity.name, "meneer-henk[bot]");
    assert_eq!(
        identity.email,
        "4242+meneer-henk[bot]@users.noreply.github.com"
    );
    assert_eq!(writer.user_login(77).await.unwrap(), "alice");
    assert!(writer.user_login(78).await.is_err(), "an unknown id");
    let credential = writer.git_credential().await.unwrap().unwrap();
    assert_eq!(credential.username, "x-access-token");
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

#[tokio::test]
async fn a_list_is_read_past_its_tenth_page() {
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
    let comments = push_writer(&server)
        .api()
        .get_all("/repos/docspec/app/pulls/7/comments")
        .await
        .unwrap();
    assert_eq!(comments.len(), 1001);
    assert_eq!(
        comments[1000]["id"], 99_999,
        "a comment past ten pages is still read"
    );
}

#[tokio::test]
async fn a_list_of_exactly_the_cap_is_read_whole() {
    let server = MockServer::start().await;
    let finding = format!("Off by one.\n\n{}", marker(MarkerKind::Finding));
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/pulls/7/comments"))
        .respond_with(move |request: &wiremock::Request| {
            let page = page_of(request);
            let mut comments = people_comments(page, page <= 100);
            if page == 100 {
                comments[99] = json!({"id": 99_999, "node_id": "PRRC_last", "path": "src/a.rs",
                    "line": 10, "body": finding, "user": {"login": "meneer-henk[bot]"}});
            }
            ResponseTemplate::new(200).set_body_json(comments)
        })
        .mount(&server)
        .await;
    let comments = push_writer(&server)
        .api()
        .get_all("/repos/docspec/app/pulls/7/comments")
        .await
        .unwrap();
    assert_eq!(
        comments.len(),
        10_000,
        "10,000 comments is the cap, not past it: the list is whole"
    );
    assert_eq!(comments[9_999]["id"], 99_999);
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
    let result = push_writer(&server)
        .api()
        .get_all("/repos/docspec/app/pulls/7/comments")
        .await;
    assert!(
        matches!(
            result,
            Err(henk_platform::PlatformError::TooMany { limit: 10_000, .. })
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_queued_review_s_check_is_created_queued_then_started_in_place() {
    let server = MockServer::start().await;
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    let writer = GitHubWriter::new(api, "meneer-henk[bot]");
    let t = target();
    let commit = CommitSha::parse(SHA).unwrap();

    Mock::given(method("POST"))
        .and(path("/repos/docspec/app/check-runs"))
        .and(body_partial_json(json!({
            "name": "Meneer Henk",
            "head_sha": SHA,
            "status": "queued",
            "details_url": "https://henk/dashboard/",
            "output": {"title": "Queued", "summary": "Waiting for a review slot."},
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 777})))
        .expect(1)
        .mount(&server)
        .await;
    let queued = writer
        .queue_review(
            &t,
            &commit,
            "Queued",
            "Waiting for a review slot.",
            "https://henk/dashboard/",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued.0, "777");

    Mock::given(method("PATCH"))
        .and(path("/repos/docspec/app/check-runs/777"))
        .and(body_partial_json(
            json!({"status": "in_progress", "details_url": "https://henk/runs/r-1"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 777})))
        .expect(1)
        .mount(&server)
        .await;
    let started = writer
        .start_review(&t, &commit, "https://henk/runs/r-1", Some(&queued))
        .await
        .unwrap();
    assert_eq!(started, Some(queued), "the same check, not a second one");
    // The POST mock expects exactly one call: none was made for the start.
}
