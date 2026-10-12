//! GitLab failing for a moment (#338), through its MCP server: what is safe
//! to repeat is tried again, and a note that may have landed is looked up
//! before it is posted again.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::needless_pass_by_value,
    missing_docs
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_mcp::testing::FakeServer;
use henk_platform::gitlab::GitLabWriter;
use henk_platform::retry::RetryPolicy;
use henk_platform::{PlatformError, PlatformWriter as _, ReviewTarget};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const BODY: &str = "No issues found.\n\n<!-- meneer-henk run=r-1 kind=summary -->";

fn text(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn gitlab_error(status: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "GitLab API error: {status}\n<html>…</html>"
    ))])
}

fn target() -> ReviewTarget {
    ReviewTarget {
        repo: RepoRef::parse(Platform::GitLab, "9xxlab/tools/cli").unwrap(),
        number: 5,
    }
}

/// A fake whose `tool` fails with `status` the first `failures` times, and
/// whose discussions hold `discussions`.
fn failing(
    tool: &'static str,
    status: &'static str,
    failures: usize,
    discussions: Value,
) -> FakeServer {
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = [
        "mr_discussions",
        "create_merge_request_note",
        "create_merge_request_discussion_note",
        "create_commit_status",
        "get_merge_request",
    ]
    .iter()
    .map(|name| FakeServer::tool(name, "", &[]))
    .collect();
    FakeServer::new(tools, move |name, args| {
        if name == tool && calls.fetch_add(1, Ordering::SeqCst) < failures {
            return gitlab_error(status);
        }
        match name {
            "mr_discussions" if args["page"] == json!(1) => text(discussions.clone()),
            "mr_discussions" => text(json!([])),
            "create_merge_request_note" => text(json!({"id": 301})),
            "create_merge_request_discussion_note" => text(json!({"id": 302})),
            "create_commit_status" => text(json!({"id": 7})),
            "get_merge_request" => text(json!({"iid": 5})),
            other => CallToolResult::error(vec![ContentBlock::text(format!("unexpected {other}"))]),
        }
    })
}

async fn writer(fake: &FakeServer) -> GitLabWriter {
    GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk").with_retry_policy(
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        },
    )
}

fn called(fake: &FakeServer, tool: &str) -> usize {
    fake.calls().iter().filter(|c| c.name == tool).count()
}

#[tokio::test]
async fn a_note_that_did_not_land_is_posted_again() {
    let fake = failing("create_merge_request_note", "502 Bad Gateway", 1, json!([]));
    let posted = writer(&fake)
        .await
        .post_comment(&target(), BODY)
        .await
        .unwrap();
    assert_eq!(posted.id, "301");
    assert_eq!(called(&fake, "create_merge_request_note"), 2);
    assert_eq!(called(&fake, "mr_discussions"), 1, "looked for it first");
}

#[tokio::test]
async fn a_note_that_landed_before_gitlab_failed_is_not_posted_again() {
    let fake = failing(
        "create_merge_request_note",
        "500 Internal Server Error",
        9,
        json!([
            {"id": "d1", "individual_note": true, "notes": [
                {"id": 400, "body": BODY, "author": {"username": "alice"}}
            ]},
            {"id": "d2", "individual_note": true, "notes": [
                {"id": 401, "body": BODY, "author": {"username": "meneerhenk"}}
            ]},
        ]),
    );
    let posted = writer(&fake)
        .await
        .post_comment(&target(), BODY)
        .await
        .unwrap();
    assert_eq!(posted.id, "401", "Henk's own, not a copy someone else made");
    assert_eq!(called(&fake, "create_merge_request_note"), 1, "posted once");
}

#[tokio::test]
async fn a_reply_that_landed_is_not_posted_again() {
    let fake = failing(
        "create_merge_request_discussion_note",
        "503 Service Unavailable",
        9,
        json!([
            {"id": "d1", "notes": [
                {"id": 500, "body": "Off by one.", "author": {"username": "alice"}},
                {"id": 501, "body": "On it.", "author": {"username": "meneerhenk"}}
            ]},
        ]),
    );
    let posted = writer(&fake)
        .await
        .reply(&target(), "d1", true, "On it.")
        .await
        .unwrap();
    assert_eq!(posted.id, "501");
    assert_eq!(called(&fake, "create_merge_request_discussion_note"), 1);
}

#[tokio::test]
async fn the_commit_status_is_set_again_when_gitlab_fails_for_a_moment() {
    let fake = failing("create_commit_status", "502 Bad Gateway", 1, json!([]));
    writer(&fake)
        .await
        .finish_review(
            &target(),
            &CommitSha::parse(SHA).unwrap(),
            None,
            &ReviewOutcome::interrupted(CommitSha::parse(SHA).unwrap()),
            "https://henk/runs/r-1",
        )
        .await
        .unwrap();
    assert_eq!(called(&fake, "create_commit_status"), 2);
}

#[tokio::test]
async fn a_lasting_refusal_is_not_tried_again() {
    let fake = failing("get_merge_request", "404 Not Found", 9, json!([]));
    let error = writer(&fake)
        .await
        .pull_request(&target())
        .await
        .unwrap_err();
    assert!(matches!(error, PlatformError::ToolFailed { .. }), "{error}");
    assert_eq!(called(&fake, "get_merge_request"), 1);
}

#[tokio::test]
async fn a_read_gives_up_with_the_last_error_when_the_attempts_run_out() {
    let fake = failing(
        "get_merge_request",
        "500 Internal Server Error",
        9,
        json!([]),
    );
    assert!(writer(&fake).await.pull_request(&target()).await.is_err());
    assert_eq!(called(&fake, "get_merge_request"), 3);
}
