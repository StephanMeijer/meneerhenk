//! GitLab writer against the in-process fake MCP server.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    missing_docs
)]

use std::sync::Arc;

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::review::{CommitSha, LaneName, LaneOutcome, LaneResult, ReviewOutcome};
use henk_domain::run::RunId;
use henk_mcp::testing::FakeServer;
use henk_platform::gitlab::GitLabWriter;
use henk_platform::{DiffSide, PlatformWriter as _, ReviewTarget};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const BASE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn marker(kind: MarkerKind) -> String {
    Marker {
        run: RunId::parse("r-1").unwrap(),
        model: ModelId::parse("m").unwrap(),
        requested_by: None,
        kind: Some(kind),
    }
    .render()
}

fn text(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn fake() -> FakeServer {
    let tools = [
        "get_merge_request",
        "mr_discussions",
        "create_merge_request_thread",
        "create_merge_request_note",
        "update_merge_request_note",
        "create_merge_request_discussion_note",
        "create_merge_request_emoji_reaction",
        "create_merge_request_note_emoji_reaction",
        "create_commit_status",
        "get_merge_request_diffs",
        "resolve_merge_request_thread",
    ]
    .iter()
    .map(|name| FakeServer::tool(name, "", &[]))
    .collect();
    FakeServer::new(tools, |name, args| match name {
        "get_merge_request" => text(json!({
            "iid": 5, "title": "Fix", "state": "opened", "draft": false, "target_branch": "main",
            "sha": SHA, "diff_refs": {"base_sha": BASE, "start_sha": BASE, "head_sha": SHA}
        })),
        "mr_discussions" => {
            if args["page"] != json!(1) {
                return text(json!([]));
            }
            text(json!([
                {"id": "d1", "notes": [
                    {"id": 101, "body": format!("Off by one.\n\n{}", marker(MarkerKind::Finding)), "author": {"username": "meneerhenk"},
                     "position": {"new_path": "src/a.rs", "new_line": 10}, "resolved": false},
                    {"id": 102, "body": "will fix", "author": {"username": "alice"}}
                ]},
                {"id": "d2", "notes": [
                    {"id": 103, "body": format!("Old.\n\n{}", marker(MarkerKind::Finding)), "author": {"username": "meneerhenk"},
                     "position": {"new_path": "src/b.rs", "new_line": 3}, "resolved": true}
                ]},
                {"id": "d3", "individual_note": true, "notes": [
                    {"id": 104, "body": format!("No issues found.\n\n{}", marker(MarkerKind::Summary)), "author": {"username": "meneerhenk"}}
                ]},
                {"id": "d4", "notes": [
                    {"id": 105, "body": "human thread", "author": {"username": "bob"}, "position": {"new_path": "x", "new_line": 1}}
                ]},
                {"id": "d5", "notes": [{"id": 106, "system": true, "body": "added 1 commit"}]}
            ]))
        }
        "create_merge_request_thread" => text(json!({"id": "d9", "notes": [{"id": 201}]})),
        "create_merge_request_note" => text(json!({"id": 301})),
        "create_merge_request_discussion_note" => text(json!({"id": 302})),
        "update_merge_request_note" => text(json!({"id": 101})),
        "resolve_merge_request_thread" => {
            text(json!({"id": args["discussion_id"], "resolved": true}))
        }
        "create_merge_request_emoji_reaction" | "create_merge_request_note_emoji_reaction" => {
            text(json!({"id": 1}))
        }
        "create_commit_status" => text(json!({"id": 7, "status": "success"})),
        "get_merge_request_diffs" => text(json!([
            {"old_path": "src/a.rs", "new_path": "src/a.rs", "new_file": false, "deleted_file": false, "renamed_file": false,
             "diff": "@@ -1,2 +1,2 @@\n-old\n+new\n keep\n"},
            {"old_path": "gone.txt", "new_path": "gone.txt", "new_file": false, "deleted_file": true, "renamed_file": false,
             "diff": "@@ -1 +0,0 @@\n-bye\n"},
            {"old_path": "a.md", "new_path": "b.md", "new_file": false, "deleted_file": false, "renamed_file": true, "diff": ""}
        ])),
        other => CallToolResult::error(vec![ContentBlock::text(format!("unexpected {other}"))]),
    })
}

fn target() -> ReviewTarget {
    ReviewTarget {
        repo: RepoRef::parse(Platform::GitLab, "9xxlab/tools/cli").unwrap(),
        number: 5,
    }
}

#[tokio::test]
async fn review_lifecycle_on_gitlab() {
    let fake = fake();
    let session = Arc::new(fake.connect("gitlab-write").await);
    let writer = GitLabWriter::new(session, "meneerhenk");
    let t = target();
    let commit = CommitSha::parse(SHA).unwrap();

    let info = writer.pull_request(&t).await.unwrap();
    assert_eq!(info.head.as_str(), SHA);
    assert_eq!(info.base_ref, "main");
    assert_eq!(
        fake.calls()[0].arguments,
        json!({"project_id": "9xxlab/tools/cli", "merge_request_iid": "5"})
    );

    assert!(
        writer
            .start_review(&t, &commit, "https://henk/runs/r-1")
            .await
            .unwrap()
            .is_none()
    );

    let findings = writer.existing_findings(&t).await.unwrap();
    assert_eq!(
        findings.len(),
        2,
        "system notes, human threads and summaries are not findings"
    );
    assert_eq!(findings[0].comment_id, "101");
    assert_eq!(findings[0].path, "src/a.rs");
    assert_eq!(findings[0].line, Some(10));
    assert!(findings[0].answered_by_person);
    assert!(!findings[0].resolved);
    assert_eq!(findings[0].node_id.as_deref(), Some("d1"));
    assert!(findings[1].resolved);

    let summaries = writer.existing_summaries(&t).await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].comment_id, "104");
    assert!(!summaries[0].folded);

    let posted = writer
        .post_finding(&t, &commit, "src/c.rs", 12, DiffSide::Right, "Wrong.")
        .await
        .unwrap();
    assert_eq!(posted.id, "201");
    assert_eq!(posted.node_id.as_deref(), Some("d9"));
    let thread_call = fake
        .calls()
        .into_iter()
        .find(|c| c.name == "create_merge_request_thread")
        .unwrap();
    assert_eq!(thread_call.arguments["position"]["head_sha"], SHA);
    assert_eq!(thread_call.arguments["position"]["new_line"], 12);
    assert_eq!(thread_call.arguments["position"]["new_path"], "src/c.rs");
    assert_eq!(thread_call.arguments["body"], "Wrong.");

    writer.fold_summary(&t, &summaries[0]).await.unwrap();
    let fold = fake
        .calls()
        .into_iter()
        .rfind(|c| c.name == "update_merge_request_note")
        .unwrap();
    assert_eq!(fold.arguments["note_id"], "104");
    assert!(
        fold.arguments["body"]
            .as_str()
            .unwrap()
            .starts_with("*Outdated.*")
    );
    assert!(
        Marker::parse(fold.arguments["body"].as_str().unwrap()).is_some(),
        "still recognisable"
    );

    let summary = writer.post_comment(&t, "1 issue found.").await.unwrap();
    assert_eq!(summary.id, "301");

    let reply = writer.reply(&t, "d1", true, "Hello.").await.unwrap();
    assert_eq!(reply.id, "302");
    let reply_call = fake
        .calls()
        .into_iter()
        .find(|c| c.name == "create_merge_request_discussion_note")
        .unwrap();
    assert_eq!(reply_call.arguments["discussion_id"], "d1");

    let outcome = ReviewOutcome {
        commit: commit.clone(),
        lanes: vec![LaneResult {
            lane: LaneName::new("a"),
            outcome: LaneOutcome::Dropped,
        }],
        open_findings: 2,
    };
    writer
        .finish_review(&t, &commit, None, &outcome, "https://henk/runs/r-1")
        .await
        .unwrap();
    let status = fake
        .calls()
        .into_iter()
        .find(|c| c.name == "create_commit_status")
        .unwrap();
    assert_eq!(
        status.arguments["state"], "success",
        "never blocks a pipeline"
    );
    assert_eq!(status.arguments["name"], "Meneer Henk");
    assert_eq!(status.arguments["description"], "Review did not complete.");
    assert_eq!(status.arguments["sha"], SHA);

    writer.acknowledge(&t, "77", false).await.unwrap();
}

#[tokio::test]
async fn tool_errors_become_platform_errors() {
    let fake = FakeServer::new(
        vec![FakeServer::tool("get_merge_request", "", &[])],
        |_, _| CallToolResult::error(vec![ContentBlock::text("404 Not Found")]),
    );
    let writer = GitLabWriter::new(Arc::new(fake.connect("gl").await), "meneerhenk");
    let error = writer.pull_request(&target()).await.unwrap_err();
    assert!(
        matches!(error, henk_platform::PlatformError::ToolFailed { ref tool, .. } if tool == "get_merge_request"),
        "{error}"
    );
}

#[tokio::test]
async fn diff_comes_from_the_merge_request_diffs_tool() {
    let fake = fake();
    let session = Arc::new(fake.connect("gitlab-write").await);
    let writer = GitLabWriter::new(session, "meneerhenk");
    let commit = CommitSha::parse(SHA).unwrap();
    let patches = writer.diff(&target(), &commit, "main").await.unwrap();
    assert_eq!(patches.len(), 3);
    assert_eq!(patches[0].status, henk_domain::diff::FileStatus::Modified);
    assert!(patches[0].patch.starts_with("@@ -1,2 +1,2 @@"));
    assert_eq!(patches[1].status, henk_domain::diff::FileStatus::Removed);
    assert_eq!(patches[1].new_path, None);
    assert_eq!(patches[2].status, henk_domain::diff::FileStatus::Renamed);
    assert_eq!(patches[2].old_path.as_deref(), Some("a.md"));
    assert_eq!(patches[2].new_path.as_deref(), Some("b.md"));
    assert_eq!(
        fake.calls()[0].arguments,
        json!({"project_id": "9xxlab/tools/cli", "merge_request_iid": "5"})
    );
}

#[tokio::test]
async fn resolve_finding_resolves_the_discussion_its_note_starts() {
    let fake = fake();
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    writer.resolve_finding(&target(), "101").await.unwrap();
    let call = fake
        .calls()
        .into_iter()
        .find(|c| c.name == "resolve_merge_request_thread")
        .unwrap();
    assert_eq!(
        call.arguments,
        json!({"project_id": "9xxlab/tools/cli", "merge_request_iid": "5", "discussion_id": "d1", "resolved": true})
    );
    assert!(
        writer.resolve_finding(&target(), "102").await.is_err(),
        "a reply starts no discussion"
    );
}
