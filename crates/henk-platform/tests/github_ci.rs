//! A pull request's CI on GitHub (#420), against wiremock.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::review::CommitSha;
use henk_platform::github::{GitHubApi, GitHubAuth, GitHubWriter};
use henk_platform::{CiConclusion, CiJobId, CiReader as _, CiStatus, PlatformError, ReviewTarget};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn target() -> ReviewTarget {
    ReviewTarget {
        repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
        number: 7,
    }
}

fn writer(server: &MockServer) -> GitHubWriter {
    let api = GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into())).unwrap();
    GitHubWriter::new(api, "meneer-henk[bot]")
}

fn commit() -> CommitSha {
    CommitSha::parse(SHA).unwrap()
}

#[tokio::test]
async fn the_jobs_are_the_check_runs_and_statuses_without_henks_own() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/docspec/app/commits/{SHA}/check-runs")))
        .and(query_param("filter", "latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 3,
            "check_runs": [
                {"id": 11, "name": "test", "status": "completed", "conclusion": "failure",
                 "started_at": "2026-10-13T10:00:00Z", "completed_at": "2026-10-13T10:05:00Z",
                 "html_url": "https://github.com/docspec/app/runs/11",
                 "app": {"slug": "github-actions", "name": "GitHub Actions"},
                 "check_suite": {"id": 5}},
                {"id": 12, "name": "lint", "status": "in_progress", "conclusion": null,
                 "app": {"slug": "github-actions", "name": "GitHub Actions"},
                 "check_suite": {"id": 5}},
                {"id": 13, "name": "Meneer Henk", "status": "in_progress", "conclusion": null,
                 "app": {"slug": "meneer-henk", "name": "Meneer Henk"},
                 "check_suite": {"id": 6}}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/actions/runs"))
        .and(query_param("head_sha", SHA))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "workflow_runs": [{"id": 1, "name": "CI", "check_suite_id": 5}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/docspec/app/commits/{SHA}/status")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "state": "pending",
            "statuses": [{"context": "ci/jenkins", "state": "pending",
                          "target_url": "https://jenkins.example/1"}]
        })))
        .mount(&server)
        .await;

    let jobs = writer(&server).jobs(&target(), &commit()).await.unwrap();
    let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
    assert_eq!(names, ["test", "lint", "ci/jenkins"]);
    assert_eq!(jobs[0].workflow.as_deref(), Some("CI"));
    assert_eq!(jobs[0].conclusion, Some(CiConclusion::Failure));
    assert_eq!(jobs[1].status, CiStatus::InProgress);
    assert!(jobs[0].has_log);
    assert!(!jobs[2].has_log);
    assert_eq!(jobs[2].id, CiJobId("status:ci/jenkins".to_owned()));
}

#[tokio::test]
async fn a_log_follows_the_redirect_without_the_token_and_keeps_its_tail() {
    let api = MockServer::start().await;
    let blobs = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/actions/jobs/11/logs"))
        .and(header("authorization", "Bearer t"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/logs/11.txt", blobs.uri())),
        )
        .mount(&api)
        .await;
    let log = (1..=1000)
        .map(|n| format!("line {n}\n"))
        .collect::<Vec<_>>()
        .concat();
    Mock::given(method("GET"))
        .and(path("/logs/11.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(log))
        .expect(1)
        .mount(&blobs)
        .await;

    let got = writer(&api)
        .job_log(&target(), &CiJobId("11".to_owned()), 100)
        .await
        .unwrap();
    assert!(got.cut);
    assert!(got.text.len() <= 100);
    assert!(got.text.ends_with("line 1000\n"), "{}", got.text);
    assert!(got.text.starts_with("line "), "starts at a whole line");

    let sent: Vec<Request> = blobs.received_requests().await.unwrap();
    assert!(
        sent.iter()
            .all(|r| !r.headers.contains_key("authorization")),
        "the token stays with GitHub"
    );
}

#[tokio::test]
async fn annotations_are_read_and_a_status_has_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/docspec/app/check-runs/11/annotations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"path": "src/lib.rs", "start_line": 3, "end_line": 3,
             "annotation_level": "failure", "title": "E0308", "message": "mismatched types"}
        ])))
        .mount(&server)
        .await;
    let writer = writer(&server);
    let notes = writer
        .annotations(&target(), &CiJobId("11".to_owned()))
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].path, "src/lib.rs");
    assert_eq!(notes[0].level, "failure");
    let status = writer
        .annotations(&target(), &CiJobId("status:ci/jenkins".to_owned()))
        .await;
    assert!(matches!(status, Err(PlatformError::Unsupported(_))));
}

#[tokio::test]
async fn without_checks_access_the_403_is_kept() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"message": "Resource not accessible by integration"})),
        )
        .mount(&server)
        .await;
    let error = writer(&server)
        .jobs(&target(), &commit())
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), Some(403));
}
