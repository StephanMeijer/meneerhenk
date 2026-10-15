//! A merge request's CI on GitLab (#420), against wiremock.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::sync::Arc;

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::review::CommitSha;
use henk_mcp::testing::FakeServer;
use henk_platform::gitlab::{GitLabRest, GitLabWriter};
use henk_platform::{CiConclusion, CiJobId, CiReader as _, PlatformError, ReviewTarget};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN: &str = "glpat-test-token";
const PROJECT: &str = "/api/v4/projects/9xxlab%2Ftools%2Fcli";

fn target() -> ReviewTarget {
    ReviewTarget {
        repo: RepoRef::parse(Platform::GitLab, "9xxlab/tools/cli").unwrap(),
        number: 5,
    }
}

async fn writer(server: Option<&MockServer>) -> (FakeServer, GitLabWriter) {
    let fake = FakeServer::new(Vec::new(), |_, _| unreachable!("no MCP call"));
    let session = Arc::new(fake.connect("gitlab-write").await);
    let writer = GitLabWriter::new(session, "meneerhenk");
    let writer = match server {
        Some(server) => writer.with_rest(
            GitLabRest::new(
                &format!("{}/api/v4", server.uri()),
                SecretString::from(TOKEN.to_owned()),
            )
            .unwrap(),
        ),
        None => writer,
    };
    (fake, writer)
}

async fn get(server: &MockServer, at: String, body: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(at))
        .and(header("PRIVATE-TOKEN", TOKEN))
        .respond_with(body)
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_jobs_are_those_of_the_pipeline_on_the_reviewed_commit() {
    let server = MockServer::start().await;
    get(
        &server,
        format!("{PROJECT}/merge_requests/5/pipelines"),
        ResponseTemplate::new(200).set_body_json(json!([
            {"id": 30, "sha": OLD, "status": "success"},
            {"id": 41, "sha": SHA, "status": "failed"},
            {"id": 40, "sha": SHA, "status": "canceled"}
        ])),
    )
    .await;
    let jobs: Vec<_> = (1..=100)
        .map(|n| json!({"id": n, "name": format!("job {n}"), "stage": "test", "status": "success"}))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("{PROJECT}/pipelines/41/jobs")))
        .and(query_param("page", "1"))
        .and(query_param("include_retried", "false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jobs))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{PROJECT}/pipelines/41/jobs")))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 101, "name": "build", "stage": "build", "status": "failed",
             "web_url": "https://gitlab.example/9xxlab/tools/cli/-/jobs/101"}
        ])))
        .mount(&server)
        .await;
    let (_fake, writer) = writer(Some(&server)).await;

    let got = writer
        .jobs(&target(), &CommitSha::parse(SHA).unwrap())
        .await
        .unwrap();
    assert_eq!(got.len(), 101, "every page is read");
    let build = got.last().unwrap();
    assert_eq!(build.id, CiJobId("101".to_owned()));
    assert_eq!(build.conclusion, Some(CiConclusion::Failure));
    assert_eq!(build.workflow.as_deref(), Some("pipeline 41, stage build"));

    let none = writer
        .jobs(&target(), &CommitSha::parse(&"b".repeat(40)).unwrap())
        .await
        .unwrap();
    assert!(none.is_empty(), "no pipeline ran on that commit");
}

#[tokio::test]
async fn a_job_trace_keeps_its_tail() {
    let server = MockServer::start().await;
    let log = (1..=500)
        .map(|n| format!("step {n}\n"))
        .collect::<Vec<_>>()
        .concat();
    get(
        &server,
        format!("{PROJECT}/jobs/101/trace"),
        ResponseTemplate::new(200).set_body_string(log),
    )
    .await;
    let (_fake, writer) = writer(Some(&server)).await;
    let got = writer
        .job_log(&target(), &CiJobId("101".to_owned()), 64)
        .await
        .unwrap();
    assert!(got.cut);
    assert!(got.text.ends_with("step 500\n"));
    assert!(got.text.starts_with("step "));
}

#[tokio::test]
async fn without_the_token_ci_is_not_read() {
    let (_fake, writer) = writer(None).await;
    let error = writer
        .jobs(&target(), &CommitSha::parse(SHA).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, PlatformError::Auth(_)), "{error}");
}
