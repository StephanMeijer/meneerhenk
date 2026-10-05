//! The tools a review lane gets besides the read-only MCP tools:
//! listing, posting and improving findings, with every rule of §3.2 and
//! §8.5 enforced here rather than in the prompt.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use henk_agent::{Tool, ToolOutput};
use henk_domain::diff::ReviewDiff;
use henk_domain::finding::{Claim, Finding, FindingKey, FindingRegistry};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::review::{CommitSha, LaneName};
use henk_domain::run::RunId;
use henk_domain::text::style_violations;
use henk_llm::{ToolDef, ToolName};
use henk_platform::{DiffSide, PlatformWriter, ReviewTarget};
use henk_store::{FindingAction, RunStore};
use serde_json::{Value, json};
use tracing::{info, warn};

/// What all three tools of one lane share.
pub struct LaneContext {
    /// The run.
    pub run: RunId,
    /// This lane.
    pub lane: LaneName,
    /// The model behind this lane, for the marker.
    pub model: ModelId,
    /// The pull/merge request.
    pub target: ReviewTarget,
    /// The reviewed commit.
    pub commit: CommitSha,
    /// Findings known on the target, shared by all lanes of the review.
    pub registry: Arc<Mutex<FindingRegistry>>,
    /// Where comments go.
    pub writer: Arc<dyn PlatformWriter>,
    /// Run records.
    pub store: Arc<RunStore>,
    /// The diff of the review, when it could be fetched. Findings are
    /// checked against it before anything is posted.
    pub diff: Option<Arc<ReviewDiff>>,
    /// The changed files this lane asked the diff of.
    pub opened: Mutex<BTreeSet<String>>,
}

impl LaneContext {
    /// The changed files this lane has not asked the diff of yet.
    #[must_use]
    pub fn unopened_files(&self) -> Vec<String> {
        let Some(diff) = &self.diff else {
            return Vec::new();
        };
        let opened = self.opened.lock().map(|o| o.clone()).unwrap_or_default();
        diff.paths()
            .filter(|p| !opened.contains(*p))
            .map(str::to_owned)
            .collect()
    }
}

/// `list_changed_files`: the files of the diff with their counts.
pub struct ListChangedFiles(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for ListChangedFiles {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_changed_files"),
            description: "Lists the files this change touches: status letter (A added, M modified, D removed, R renamed), path, lines added and removed. Start here, then read each file's diff with get_file_diff.".to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        match &self.0.diff {
            Some(diff) => ToolOutput::ok(diff.render_list()),
            None => ToolOutput::error(
                "The diff could not be fetched for this review; use pull_request_read with method get_files instead.",
            ),
        }
    }
}

/// `get_file_diff`: one file's hunks with line numbers on both sides.
pub struct GetFileDiff(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for GetFileDiff {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("get_file_diff"),
            description: "The diff of one changed file, every line numbered: old line number, new line number, then + for added, - for removed, space for context. A finding goes on a line shown here, by its new line number (or old number with side LEFT for a removed line).".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "A path from list_changed_files"}
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let Some(path) = arg_str(&args, "path") else {
            return ToolOutput::error("path is required");
        };
        let Some(diff) = &self.0.diff else {
            return ToolOutput::error(
                "The diff could not be fetched for this review; use pull_request_read with method get_diff instead.",
            );
        };
        match diff.file(path) {
            Some(file) => {
                if let Ok(mut opened) = self.0.opened.lock() {
                    opened.insert(file.path.clone());
                }
                ToolOutput::ok(file.render())
            }
            None => ToolOutput::error(format!(
                "{path} is not part of this change. The changed files are:\n{}",
                diff.render_list()
            )),
        }
    }
}

/// `read_file`: a line range of a file at the reviewed commit, numbered.
/// Wraps the guarded MCP file read so a lane never pulls whole files.
pub struct ReadFile {
    /// The guarded `get_file_contents` of the platform session.
    pub inner: Arc<dyn Tool>,
}

/// Lines per read.
const READ_FILE_MAX_LINES: usize = 400;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("read_file"),
            description: format!(
                "Reads a line range of a file at the reviewed commit, numbered. Use it to confirm a suspicion from the diff: callers, tests, definitions. At most {READ_FILE_MAX_LINES} lines per call; ask for the range you need."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path in the repository"},
                    "start_line": {"type": "integer", "description": "First line, 1-based (default 1)"},
                    "end_line": {"type": "integer", "description": format!("Last line, inclusive (default start_line + {})", READ_FILE_MAX_LINES - 1)}
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let Some(path) = arg_str(&args, "path") else {
            return ToolOutput::error("path is required");
        };
        let start = args
            .get("start_line")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
            .unwrap_or(1);
        let end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n >= start)
            .unwrap_or(start + READ_FILE_MAX_LINES - 1)
            .min(start + READ_FILE_MAX_LINES - 1);
        let output = self.inner.call(json!({"path": path})).await;
        if output.is_error {
            return output;
        }
        ToolOutput::ok(number_lines(&output.content, start, end))
    }
}

/// Numbers `content` from `start` to `end` inclusive, after dropping the
/// MCP server's preamble (the status line and the resource URI).
fn number_lines(content: &str, start: usize, end: usize) -> String {
    let mut lines: Vec<&str> = content.lines().collect();
    if lines
        .first()
        .is_some_and(|l| l.starts_with("successfully downloaded"))
    {
        lines.remove(0);
    }
    if lines.first().is_some_and(|l| l.starts_with("[repo://")) {
        lines.remove(0);
    }
    let total = lines.len();
    if start > total {
        return format!("The file has {total} lines; nothing at line {start}.");
    }
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate().skip(start - 1) {
        let number = index + 1;
        if number > end {
            break;
        }
        let _ = writeln!(out, "{number:>5}| {line}");
    }
    if end < total {
        let _ = writeln!(out, "[lines {}-{total} not shown]", end + 1);
    }
    out
}

impl LaneContext {
    fn marker(&self) -> Marker {
        Marker {
            run: self.run.clone(),
            model: self.model.clone(),
            requested_by: None,
            kind: Some(MarkerKind::Finding),
        }
    }

    fn style_error(body: &str) -> Option<ToolOutput> {
        let violations = style_violations(body);
        if violations.is_empty() {
            return None;
        }
        Some(ToolOutput::error(format!(
            "The text breaks the style rules ({} problem(s): no emoji, no em-dash). Rewrite it and try again.",
            violations.len()
        )))
    }
}

fn name(name: &str) -> ToolName {
    ToolName::parse(name).unwrap_or_else(|_| unreachable!("tool names here are constants"))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `list_existing_findings`: what is already on the pull request.
pub struct ListExistingFindings(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for ListExistingFindings {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_existing_findings"),
            description: "Lists the findings already posted on this pull request by any reviewer, with their comment ids, locations and text. Call this before posting.".to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        let Ok(registry) = self.0.registry.lock() else {
            return ToolOutput::error("finding registry unavailable");
        };
        if registry.is_empty() {
            return ToolOutput::ok("No findings yet.");
        }
        let lines: Vec<String> = registry
            .iter()
            .map(|f| {
                let state = match (f.resolved, f.answered_by_person, f.in_diff) {
                    (true, _, _) => " [resolved]",
                    (_, true, _) => " [answered by a person; do not rewrite]",
                    (_, _, false) => " [outdated]",
                    _ => "",
                };
                let body = f.body.split("<!--").next().unwrap_or("").trim();
                format!(
                    "- comment {} at {}:{}{state}\n  {}",
                    f.comment_id, f.key.path, f.key.line, body
                )
            })
            .collect();
        ToolOutput::ok(lines.join("\n"))
    }
}

/// `post_finding`: one problem on one line.
pub struct PostFinding(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for PostFinding {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("post_finding"),
            description: "Posts one finding as a comment on one line of the diff at the reviewed commit. Only for a real problem this change introduces. One finding per line; if the line already has one, improve it instead.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path as in the diff"},
                    "line": {"type": "integer", "description": "Line number in the new version of the file (or the old version when side is LEFT)"},
                    "side": {"type": "string", "enum": ["LEFT", "RIGHT"], "description": "RIGHT (default) for added or changed lines, LEFT for removed lines"},
                    "body": {"type": "string", "description": "The finding: what is wrong, why it matters, what would fix it"}
                },
                "required": ["path", "line", "body"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(path), Some(body)) = (arg_str(&args, "path"), arg_str(&args, "body")) else {
            return ToolOutput::error("path and body are required");
        };
        let Some(line) = args
            .get("line")
            .and_then(Value::as_u64)
            .and_then(|l| u32::try_from(l).ok())
            .filter(|l| *l > 0)
        else {
            return ToolOutput::error("line must be a positive integer");
        };
        let side = match arg_str(&args, "side") {
            Some("LEFT") => DiffSide::Left,
            _ => DiffSide::Right,
        };
        if let Some(error) = LaneContext::style_error(body) {
            return error;
        }
        if let Some(diff) = &ctx.diff
            && let Err(reason) = diff.commentable(path, line, side)
        {
            return ToolOutput::error(format!(
                "Cannot post on {path}:{line}: {reason}. Use get_file_diff to see the numbered lines."
            ));
        }
        let key = FindingKey {
            path: path.to_owned(),
            line,
        };

        // Claim under the lock, then release it before any I/O: a std mutex
        // guard must not live across an await. Two lanes racing between the
        // claim and the post is rare, and the registry stays consistent
        // because the post result is recorded under the lock again.
        let taken: Option<String> = {
            let Ok(registry) = ctx.registry.lock() else {
                return ToolOutput::error("finding registry unavailable");
            };
            match registry.claim(&key) {
                Claim::Exists(existing) => Some(existing.comment_id.clone()),
                Claim::New => None,
            }
        };
        if let Some(existing_id) = taken {
            let _ = ctx.store.record_finding(
                &ctx.run,
                ctx.lane.as_str(),
                path,
                line,
                &existing_id,
                FindingAction::Refused,
            );
            return ToolOutput::error(format!(
                "Line {path}:{line} already has finding {existing_id}. Improve it with improve_finding if you can explain it better, otherwise move on."
            ));
        }

        let full_body = ctx.marker().attach(body);
        let posted = match ctx
            .writer
            .post_finding(&ctx.target, &ctx.commit, path, line, side, &full_body)
            .await
        {
            Ok(posted) => posted,
            Err(error) => {
                warn!(%error, path, line, "posting a finding failed");
                // On the timeline too, so a lane whose every post failed does
                // not look like a lane that found nothing.
                let _ = ctx.store.event(
                    &ctx.run,
                    "warn",
                    &format!(
                        "{}: could not post a finding on {path}:{line}: {error}",
                        ctx.lane
                    ),
                );
                return ToolOutput::error(format!(
                    "Could not post on {path}:{line}: {error}. If the line is not part of the diff, pick a line that is."
                ));
            }
        };
        if let Ok(mut registry) = ctx.registry.lock() {
            registry.record(Finding {
                key,
                comment_id: posted.id.clone(),
                body: full_body,
                lane: Some(ctx.lane.clone()),
                answered_by_person: false,
                resolved: false,
                in_diff: true,
            });
        }
        let _ = ctx.store.record_finding(
            &ctx.run,
            ctx.lane.as_str(),
            path,
            line,
            &posted.id,
            FindingAction::Posted,
        );
        info!(lane = %ctx.lane, path, line, comment = %posted.id, "finding posted");
        ToolOutput::ok(format!("Posted as comment {}.", posted.id))
    }
}

/// `improve_finding`: rewrite an existing finding to explain it better.
pub struct ImproveFinding(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for ImproveFinding {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("improve_finding"),
            description: "Replaces the text of an existing finding when you can explain the same problem better. Never changes a finding a person has already answered.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "comment_id": {"type": "string", "description": "The comment id from list_existing_findings"},
                    "body": {"type": "string", "description": "The improved finding"}
                },
                "required": ["comment_id", "body"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(comment_id), Some(body)) = (arg_str(&args, "comment_id"), arg_str(&args, "body"))
        else {
            return ToolOutput::error("comment_id and body are required");
        };
        if let Some(error) = LaneContext::style_error(body) {
            return error;
        }
        let existing = match ctx.registry.lock() {
            Ok(registry) => registry
                .iter()
                .find(|f| f.comment_id == comment_id)
                .cloned(),
            Err(_) => return ToolOutput::error("finding registry unavailable"),
        };
        let Some(existing) = existing else {
            return ToolOutput::error(format!("No finding with comment id {comment_id}."));
        };
        if existing.answered_by_person {
            return ToolOutput::error("A person has answered that finding; it stays as it is.");
        }
        let full_body = ctx.marker().attach(body);
        if let Err(error) = ctx
            .writer
            .update_finding(&ctx.target, comment_id, &full_body)
            .await
        {
            return ToolOutput::error(format!("Could not update comment {comment_id}: {error}"));
        }
        if let Ok(mut registry) = ctx.registry.lock() {
            registry.improve(&existing.key, full_body, Some(ctx.lane.clone()));
        }
        let _ = ctx.store.record_finding(
            &ctx.run,
            ctx.lane.as_str(),
            &existing.key.path,
            existing.key.line,
            comment_id,
            FindingAction::Improved,
        );
        ToolOutput::ok(format!("Updated comment {comment_id}."))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::diff::ReviewDiff;
    use henk_domain::review::LaneName;
    use henk_llm::ToolName;

    use super::*;
    use crate::listeners::testing::FakeWriter;

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn main() {
-    let x = 1;
+    let x = 2;
 }
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -5 +5 @@
-old
+new
";

    fn context(diff: Option<ReviewDiff>) -> Arc<LaneContext> {
        let store = Arc::new(RunStore::in_memory().unwrap());
        let run = RunId::parse("r-1").unwrap();
        store
            .create_run(&henk_store::NewRun {
                id: run.clone(),
                kind: henk_domain::run::RunKind::Review,
                platform: Platform::GitHub,
                repo: "o/r".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "test".into(),
                link: "l".into(),
            })
            .unwrap();
        Arc::new(LaneContext {
            run,
            lane: LaneName::new("lane-a"),
            model: ModelId::parse("m").unwrap(),
            target: ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 7,
            },
            commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
            registry: Arc::new(Mutex::new(FindingRegistry::seeded(std::iter::empty()))),
            writer: Arc::new(FakeWriter::default()),
            store,
            diff: diff.map(Arc::new),
            opened: Mutex::new(BTreeSet::new()),
        })
    }

    struct FixedFile(&'static str);

    #[async_trait::async_trait]
    impl Tool for FixedFile {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: ToolName::parse("github__get_file_contents").unwrap(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
            }
        }

        async fn call(&self, args: Value) -> ToolOutput {
            assert_eq!(args["path"], "src/a.rs");
            ToolOutput::ok(self.0)
        }
    }

    #[tokio::test]
    async fn list_and_file_diff_track_what_the_lane_opened() {
        let ctx = context(Some(ReviewDiff::from_unified(DIFF)));
        let list = ListChangedFiles(Arc::clone(&ctx)).call(json!({})).await;
        assert!(!list.is_error);
        assert!(list.content.contains("M src/a.rs (+1 -1)"));
        assert_eq!(ctx.unopened_files(), vec!["src/a.rs", "README.md"]);

        let file = GetFileDiff(Arc::clone(&ctx))
            .call(json!({"path": "src/a.rs"}))
            .await;
        assert!(!file.is_error, "{file:?}");
        assert!(
            file.content.contains("    2 |      |-    let x = 1;"),
            "{}",
            file.content
        );
        assert_eq!(ctx.unopened_files(), vec!["README.md"]);

        let missing = GetFileDiff(Arc::clone(&ctx))
            .call(json!({"path": "src/zzz.rs"}))
            .await;
        assert!(missing.is_error);
        assert!(missing.content.contains("README.md"));

        let none = context(None);
        assert!(ListChangedFiles(none).call(json!({})).await.is_error);
    }

    #[tokio::test]
    async fn read_file_numbers_a_range_and_drops_the_preamble() {
        let text = "successfully downloaded text file (SHA: abc)\n[repo://o/r/sha/x/contents/src/a.rs]\nline one\nline two\nline three\nline four\n";
        let tool = ReadFile {
            inner: Arc::new(FixedFile(text)),
        };
        let all = tool.call(json!({"path": "src/a.rs"})).await;
        assert_eq!(
            all.content,
            "    1| line one\n    2| line two\n    3| line three\n    4| line four\n"
        );
        let range = tool
            .call(json!({"path": "src/a.rs", "start_line": 2, "end_line": 3}))
            .await;
        assert_eq!(
            range.content,
            "    2| line two\n    3| line three\n[lines 4-4 not shown]\n"
        );
        let past = tool
            .call(json!({"path": "src/a.rs", "start_line": 9}))
            .await;
        assert!(past.content.contains("has 4 lines"));
        assert!(tool.call(json!({})).await.is_error);
    }

    #[tokio::test]
    async fn post_finding_refuses_lines_outside_the_diff_before_posting() {
        let ctx = context(Some(ReviewDiff::from_unified(DIFF)));
        let tool = PostFinding(Arc::clone(&ctx));
        let outside = tool
            .call(json!({"path": "src/a.rs", "line": 40, "body": "Wrong."}))
            .await;
        assert!(outside.is_error);
        assert!(outside.content.contains("1-3"), "{}", outside.content);
        let unknown = tool
            .call(json!({"path": "lib.rs", "line": 1, "body": "Wrong."}))
            .await;
        assert!(
            unknown.content.contains("not part of the diff"),
            "{}",
            unknown.content
        );
        // A line in the diff reaches the writer (the fake refuses, which is
        // a different error) and lands on the timeline.
        let inside = tool
            .call(json!({"path": "src/a.rs", "line": 2, "body": "Wrong."}))
            .await;
        assert!(
            inside.content.contains("not in the fake"),
            "{}",
            inside.content
        );
        assert_eq!(ctx.store.events(&ctx.run).unwrap().len(), 1);
        // Without a diff nothing is checked here.
        let unchecked = PostFinding(context(None))
            .call(json!({"path": "src/a.rs", "line": 40, "body": "Wrong."}))
            .await;
        assert!(unchecked.content.contains("not in the fake"));
    }
}
