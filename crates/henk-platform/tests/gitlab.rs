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
        checked_by: None,
        withdrawn: None,
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
                {"id": "d6", "individual_note": true, "notes": [
                    {"id": 107, "body": format!("Review did not complete.\n\n{}", marker(MarkerKind::Failure)), "author": {"username": "meneerhenk"}}
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
    assert_eq!(summaries.len(), 2, "summaries and failure comments fold");
    assert_eq!(summaries[0].comment_id, "104");
    assert!(!summaries[0].folded);
    assert_eq!(summaries[1].comment_id, "107");

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
        nothing_to_review: false,
        superseded: false,
        interrupted: false,
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

/// A work-item tracker: one item #9 of `kind` with labels and a weight,
/// answering the way `@zereight/mcp-gitlab` does (GraphQL iids are strings).
fn work_items(kind: &'static str) -> FakeServer {
    let tools = [
        "get_work_item",
        "update_work_item",
        "create_work_item",
        "convert_work_item_type",
        "create_work_item_note",
    ]
    .iter()
    .map(|name| FakeServer::tool(name, "", &[]))
    .collect();
    FakeServer::new(tools, move |name, args| match name {
        "get_work_item" => text(json!({
            "id": "gid://gitlab/WorkItem/4242", "iid": "9", "title": "Export runs", "state": "OPEN",
            "type": kind, "webUrl": "https://gitlab.example/g/p/-/work_items/9",
            "description": "Body.", "labels": ["backend", "old"], "weight": 5,
            "dueDate": "2026-12-01", "healthStatus": "needsAttention"
        })),
        "create_work_item" => text(json!({
            "id": "gid://gitlab/WorkItem/4300", "iid": "12", "title": args["title"],
            "type": if args["type"] == json!("task") { "Task" } else { "Issue" },
            "webUrl": "https://gitlab.example/g/p/-/work_items/12"
        })),
        "create_work_item_note" => {
            text(json!({"id": "gid://gitlab/Note/77", "body": args["body"]}))
        }
        "update_work_item" | "convert_work_item_type" => text(json!({"iid": "9"})),
        other => CallToolResult::error(vec![ContentBlock::text(format!("unexpected {other}"))]),
    })
}

fn issue_target() -> henk_platform::IssueTarget {
    henk_platform::IssueTarget {
        repo: RepoRef::parse(Platform::GitLab, "g/p").unwrap(),
        number: 9,
    }
}

fn last_call(fake: &FakeServer, tool: &str) -> Value {
    fake.calls()
        .into_iter()
        .rev()
        .find(|c| c.name == tool)
        .unwrap_or_else(|| panic!("no {tool} call"))
        .arguments
}

#[tokio::test]
async fn a_work_item_reads_as_an_issue_with_its_fields() {
    use henk_domain::triage::{Health, date_text};
    use henk_platform::IssueWriter as _;

    let fake = work_items("Issue");
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    let issue = writer.issue(&issue_target()).await.unwrap();
    assert_eq!(issue.number, 9, "a string iid is read");
    assert_eq!(issue.id, Some(4242));
    assert!(issue.open);
    assert_eq!(issue.kind.as_deref(), Some("Issue"));
    assert_eq!(issue.labels, ["backend", "old"]);
    assert_eq!(issue.body, "Body.");
    assert_eq!(issue.fields.weight, Some(5));
    assert_eq!(
        issue.fields.due.map(date_text).as_deref(),
        Some("2026-12-01")
    );
    assert_eq!(issue.fields.health, Some(Health::NeedsAttention));
    assert_eq!(issue.fields.start, None);
    assert_eq!(
        last_call(&fake, "get_work_item"),
        json!({"project_id": "g/p", "iid": 9})
    );
}

#[tokio::test]
async fn an_update_sends_label_changes_type_and_fields_as_work_item_calls() {
    use henk_domain::triage::TriageFields;
    use henk_platform::{IssueUpdate, IssueWriter as _};

    let fake = work_items("Issue");
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    writer
        .update_issue(
            &issue_target(),
            IssueUpdate {
                title: Some("Export runs as CSV".to_owned()),
                labels: Some(vec!["backend".to_owned(), "priority::high".to_owned()]),
                issue_type: Some("Task".to_owned()),
                fields: Some(
                    TriageFields::parse(Some(3), Some("2026-11-02"), None, Some("on_track"))
                        .unwrap(),
                ),
                ..IssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        last_call(&fake, "convert_work_item_type"),
        json!({"project_id": "g/p", "iid": 9, "new_type": "task"})
    );
    assert_eq!(
        last_call(&fake, "update_work_item"),
        json!({
            "project_id": "g/p", "iid": 9, "title": "Export runs as CSV",
            "add_labels": ["priority::high"], "remove_labels": ["old"],
            "weight": 3, "start_date": "2026-11-02", "health_status": "onTrack"
        })
    );

    let before = fake.calls().len();
    writer
        .update_issue(&issue_target(), IssueUpdate::default())
        .await
        .unwrap();
    assert_eq!(
        fake.calls().len(),
        before,
        "nothing to change sends nothing"
    );

    let refused = writer
        .update_issue(
            &issue_target(),
            IssueUpdate {
                issue_type: Some("incident".to_owned()),
                ..IssueUpdate::default()
            },
        )
        .await;
    assert!(matches!(
        refused,
        Err(henk_platform::PlatformError::Unsupported(_))
    ));
}

#[tokio::test]
async fn every_relation_is_one_work_item_update() {
    use henk_platform::{IssueRelation, IssueWriter as _};

    let fake = work_items("Issue");
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    for (relation, expected) in [
        (IssueRelation::Parent, json!({"parent_iid": 4})),
        (
            IssueRelation::SubIssue,
            json!({"children_to_add": [{"iid": 4}]}),
        ),
        (
            IssueRelation::Blocks,
            json!({"linked_items_to_add": [{"iid": 4, "link_type": "BLOCKS"}]}),
        ),
        (
            IssueRelation::BlockedBy,
            json!({"linked_items_to_add": [{"iid": 4, "link_type": "BLOCKED_BY"}]}),
        ),
        (
            IssueRelation::RelatesTo,
            json!({"linked_items_to_add": [{"iid": 4, "link_type": "RELATED"}]}),
        ),
    ] {
        writer
            .link_issues(&issue_target(), relation, 4)
            .await
            .unwrap();
        let mut want = json!({"project_id": "g/p", "iid": 9});
        want.as_object_mut()
            .unwrap()
            .extend(expected.as_object().unwrap().clone());
        assert_eq!(last_call(&fake, "update_work_item"), want, "{relation:?}");
    }
}

#[tokio::test]
async fn a_sub_issue_is_a_task_made_under_its_parent_in_one_call() {
    use henk_platform::IssueWriter as _;

    let fake = work_items("Issue");
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    let created = writer
        .create_sub_issue(&issue_target(), "Export CSV", "The CSV half.")
        .await
        .unwrap();
    assert_eq!(created.issue.number, 12);
    assert_eq!(created.issue.kind.as_deref(), Some("Task"));
    assert_eq!(created.unlinked, None);
    assert_eq!(
        last_call(&fake, "create_work_item"),
        json!({"project_id": "g/p", "title": "Export CSV", "description": "The CSV half.", "type": "task", "parent_iid": 9})
    );
    assert!(
        !fake.calls().iter().any(|c| c.name == "update_work_item"),
        "no separate link call"
    );

    let fake = work_items("Task");
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    let task = writer.issue(&issue_target()).await.unwrap();
    assert!(matches!(
        writer.may_have_children(&task),
        Err(henk_platform::PlatformError::Unsupported(_))
    ));
    let refused = writer
        .create_sub_issue(&issue_target(), "Export CSV", "The CSV half.")
        .await;
    assert!(matches!(
        refused,
        Err(henk_platform::PlatformError::Unsupported(_))
    ));
    assert!(!fake.calls().iter().any(|c| c.name == "create_work_item"));
}

#[tokio::test]
async fn a_comment_is_a_work_item_note() {
    use henk_platform::IssueWriter as _;

    let fake = work_items("Task");
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    let posted = writer
        .comment(&issue_target(), "Two questions.")
        .await
        .unwrap();
    assert_eq!(posted.id, "gid://gitlab/Note/77");
    assert_eq!(
        last_call(&fake, "create_work_item_note"),
        json!({"project_id": "g/p", "iid": 9, "body": "Two questions."})
    );
}

/// `list_labels` with 100 labels on page 1 and one scoped label on page 2;
/// a third page is an error.
fn labels() -> FakeServer {
    let tools = vec![FakeServer::tool("list_labels", "", &[])];
    FakeServer::new(tools, |name, args| match (name, args["page"].as_u64()) {
        ("list_labels", Some(1)) => text(Value::Array(
            (1..=100)
                .map(|n| json!({"name": format!("label-{n}")}))
                .collect(),
        )),
        ("list_labels", Some(2)) => text(json!([{"name": "priority::high"}])),
        (other, page) => CallToolResult::error(vec![ContentBlock::text(format!(
            "unexpected {other} page {page:?}"
        ))]),
    })
}

#[tokio::test]
async fn labels_are_read_across_pages() {
    use henk_platform::IssueWriter as _;

    let fake = labels();
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    let repo = RepoRef::parse(Platform::GitLab, "g/p").unwrap();
    let names = writer.repo_labels(&repo).await.unwrap();
    assert_eq!(names.len(), 101);
    assert!(names.iter().any(|n| n == "priority::high"));
    let calls: Vec<Value> = fake
        .calls()
        .into_iter()
        .filter(|c| c.name == "list_labels")
        .map(|c| c.arguments)
        .collect();
    assert_eq!(
        calls,
        [
            json!({"project_id": "g/p", "per_page": 100, "page": 1}),
            json!({"project_id": "g/p", "per_page": 100, "page": 2}),
        ]
    );
}

#[tokio::test]
async fn a_short_items_page_is_the_last() {
    use henk_platform::IssueWriter as _;

    let tools = vec![FakeServer::tool("list_labels", "", &[])];
    let fake = FakeServer::new(tools, |_, _| {
        text(json!({"items": [{"name": "bug"}, {"name": "team::core"}]}))
    });
    let writer = GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk");
    let repo = RepoRef::parse(Platform::GitLab, "g/p").unwrap();
    let names = writer.repo_labels(&repo).await.unwrap();
    assert_eq!(names, ["bug", "team::core"]);
    assert_eq!(fake.calls().len(), 1);
}
