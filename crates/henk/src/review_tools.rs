//! The tools a review lane gets besides the read-only MCP tools:
//! listing findings and drafting new ones, rewrites and withdrawals, with
//! every rule of §3.2 and §8.5 enforced here rather than in the prompt. A
//! lane never writes to the platform: its writes are drafts, checked
//! together after the lanes and written by [`crate::drafts`] (#189).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{Continuation, EndReason, Ending, Tool, ToolOutput};
use henk_domain::diff::ReviewDiff;
use henk_domain::draft::{DraftBook, DraftKind};
use henk_domain::finding::{Claim, FindingKey, FindingRegistry};
use henk_domain::marker::ModelId;
use henk_domain::review::LaneName;
use henk_domain::run::RunId;
use henk_domain::text::style_violations;
use henk_llm::{ToolDef, ToolName};
use henk_platform::DiffSide;
use henk_store::{DraftRecord, FindingAction, RunStore};
use serde_json::{Value, json};
use tracing::info;

/// The diff of a review as the read tools serve it, and which files a
/// session asked the diff of.
pub struct DiffFiles {
    /// The diff.
    pub diff: Arc<ReviewDiff>,
    opened: Mutex<BTreeSet<String>>,
}

impl DiffFiles {
    /// Serves `diff`, nothing opened yet.
    #[must_use]
    pub fn new(diff: Arc<ReviewDiff>) -> Self {
        Self {
            diff,
            opened: Mutex::new(BTreeSet::new()),
        }
    }

    /// The changed files not asked the diff of yet.
    #[must_use]
    pub fn unopened(&self) -> Vec<String> {
        let opened = self.opened.lock().map(|o| o.clone()).unwrap_or_default();
        self.diff
            .paths()
            .filter(|p| !opened.contains(*p))
            .map(str::to_owned)
            .collect()
    }
}

/// What the tools of one lane share.
pub struct LaneContext {
    /// The run.
    pub run: RunId,
    /// This lane.
    pub lane: LaneName,
    /// The model behind this lane, for the marker.
    pub model: ModelId,
    /// Findings known on the target, shared by all lanes of the review.
    pub registry: Arc<Mutex<FindingRegistry>>,
    /// The review's drafts, shared by all lanes (#189).
    pub drafts: Arc<Mutex<DraftBook>>,
    /// Run records.
    pub store: Arc<dyn RunStore>,
    /// The diff of the review. Findings are checked against it before
    /// anything is drafted.
    pub files: Arc<DiffFiles>,
    /// This lane's own workspace at the reviewed commit, when its profile
    /// reviews in one (#170). Never exported.
    pub workspace: Option<Arc<dyn crate::workspace::Workspace>>,
}

impl LaneContext {
    /// The changed files this lane has not asked the diff of yet.
    #[must_use]
    pub fn unopened_files(&self) -> Vec<String> {
        self.files.unopened()
    }
}

impl LaneContext {
    /// The refusal when `key` already has a finding on the pull request:
    /// improve it instead.
    ///
    /// This only reads the registry. Lanes do not race for a line here:
    /// [`DraftBook::add`] in [`LaneContext::queue`] takes it under the
    /// drafts' lock, and nothing is posted before every lane is done
    /// (#49, #189).
    async fn refuse_if_taken(&self, key: &FindingKey) -> Option<ToolOutput> {
        let taken: Option<String> = {
            let Ok(registry) = self.registry.lock() else {
                return Some(ToolOutput::error("finding registry unavailable"));
            };
            match registry.claim(key) {
                Claim::Exists(existing) => Some(existing.comment_id.clone()),
                Claim::New => None,
            }
        };
        let existing_id = taken?;
        let (path, line) = (&key.path, key.line);
        let _ = self
            .store
            .record_finding(
                &self.run,
                self.lane.as_str(),
                path,
                line,
                &existing_id,
                FindingAction::Refused,
            )
            .await;
        Some(ToolOutput::error(format!(
            "Line {path}:{line} already has finding {existing_id}. Improve it with improve_finding if you can explain it better, otherwise move on."
        )))
    }

    /// Adds a draft and tells the lane what became of it: queued,
    /// replacing its own earlier draft on that line, or refused because
    /// another lane's draft holds the line.
    async fn queue(&self, kind: DraftKind, key: FindingKey, text: &str) -> ToolOutput {
        let added = match self.drafts.lock() {
            Ok(mut drafts) => {
                let replacing = drafts.iter().any(|d| d.key == key);
                drafts
                    .add(&self.lane, &self.model, kind.clone(), key.clone(), text)
                    .map(|id| (id, replacing))
            }
            Err(_) => return ToolOutput::error("drafts unavailable"),
        };
        let (path, line) = (&key.path, key.line);
        match added {
            Ok((id, replacing)) => {
                let record = DraftRecord {
                    at: String::new(),
                    draft: id.to_string(),
                    lane: self.lane.as_str().to_owned(),
                    model: self.model.as_str().to_owned(),
                    kind: kind.as_str().to_owned(),
                    path: path.clone(),
                    line,
                    target: kind.comment_id().unwrap_or_default().to_owned(),
                    body: text.to_owned(),
                    decision: None,
                };
                if let Err(error) = self.store.record_draft(&self.run, &record).await {
                    tracing::warn!(%error, draft = %id, "could not record a draft");
                }
                info!(lane = %self.lane, draft = %id, kind = kind.as_str(), path = %path, line, "draft queued");
                let what = if replacing {
                    format!("Replaced your draft {id} on {path}:{line}.")
                } else {
                    format!("Queued as draft {id}.")
                };
                ToolOutput::ok(format!(
                    "{what} Every draft is checked after all reviewers are done and written only if it holds; you will not hear the verdict. Move on."
                ))
            }
            Err(taken) => {
                let _ = self
                    .store
                    .record_finding(
                        &self.run,
                        self.lane.as_str(),
                        path,
                        line,
                        kind.comment_id().unwrap_or_default(),
                        FindingAction::Refused,
                    )
                    .await;
                ToolOutput::error(format!(
                    "{path}:{line} already has draft {} by another reviewer, waiting for the fact-check. If yours is the same problem, move on; if it is a different one, put it on another line it concerns.",
                    taken.id
                ))
            }
        }
    }
}

/// What a lane is told, once each, before it is allowed to end: an answer
/// cut off at the output cap gets one chance to post what it was sure of,
/// and an end of turn with changed files never opened gets one request to
/// look at them. After that the lane ends on its own terms.
#[must_use]
pub fn lane_continuation(context: Arc<LaneContext>) -> Continuation {
    let nudged_cap = AtomicBool::new(false);
    let nudged_coverage = AtomicBool::new(false);
    Box::new(move |ending: &Ending<'_>| match ending.reason {
        EndReason::OutputCap => {
            if nudged_cap.swap(true, Ordering::SeqCst) {
                return None;
            }
            info!(lane = %context.lane, turn = ending.turn, "nudging after an output cap");
            Some(
                "Your answer was cut off at the output limit. Draft each finding you are sure of with post_finding, one call per finding, then end your turn.".to_owned(),
            )
        }
        EndReason::EndTurn => {
            let unopened = context.unopened_files();
            if unopened.is_empty() || nudged_coverage.swap(true, Ordering::SeqCst) {
                return None;
            }
            info!(lane = %context.lane, turn = ending.turn, files = unopened.len(), "nudging to cover the remaining files");
            let shown: Vec<&str> = unopened.iter().take(20).map(String::as_str).collect();
            let more = if unopened.len() > shown.len() {
                format!(" and {} more", unopened.len() - shown.len())
            } else {
                String::new()
            };
            Some(format!(
                "You ended without looking at {} changed file(s): {}{more}. Look at each with get_file_diff and decide, or end your turn if you are done.",
                unopened.len(),
                shown.join(", ")
            ))
        }
    })
}

/// `list_changed_files`: the files of the diff with their counts.
pub struct ListChangedFiles(pub Arc<DiffFiles>);

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
        ToolOutput::ok(self.0.diff.render_list())
    }
}

/// `get_file_diff`: the hunks of one or more changed files, with line
/// numbers on both sides.
pub struct GetFileDiff(pub Arc<DiffFiles>);

/// Characters of diff one `get_file_diff` call returns at most; the files
/// that would pass it are named so the model can ask for them next. The
/// first file asked for is always returned.
const FILE_DIFF_BATCH_CHARS: usize = 40_000;

#[async_trait::async_trait]
impl Tool for GetFileDiff {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("get_file_diff"),
            description: "The diff of changed files, every line numbered: old line number, new line number, then + for added, - for removed, space for context. Pass several paths at once to read them in one call. A finding goes on a line shown here, by its new line number (or old number with side LEFT for a removed line).".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Paths from list_changed_files; several at once"},
                    "path": {"type": "string", "description": "One path from list_changed_files"}
                }
            }),
        }
    }

    /// The diff is what a lane reviews: other results are stubbed first.
    fn keep_in_context(&self) -> bool {
        true
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let mut paths: Vec<&str> = args
            .get("paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if let Some(path) = arg_str(&args, "path") {
            paths.insert(0, path);
        }
        if paths.is_empty() {
            return ToolOutput::error("paths (or path) is required");
        }
        match render_files(&self.0, &paths, FILE_DIFF_BATCH_CHARS) {
            Ok(text) => ToolOutput::ok(text),
            Err(text) => ToolOutput::error(text),
        }
    }
}

/// The diffs of `paths`, in order, under a header each, until `cap`
/// characters; unknown paths get a note. Marks every returned file opened.
/// An error when no path is part of the change.
fn render_files(files: &DiffFiles, paths: &[&str], cap: usize) -> Result<String, String> {
    let diff = &files.diff;
    let mut out = String::new();
    let mut unknown = Vec::new();
    let mut left_out = Vec::new();
    let mut returned = 0_usize;
    for path in paths {
        let Some(file) = diff.file(path) else {
            unknown.push(*path);
            continue;
        };
        let rendered = file.render();
        if returned > 0 && out.len() + rendered.len() > cap {
            left_out.push(file.path.as_str());
            continue;
        }
        if returned > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "== {} ==", file.path);
        out.push_str(&rendered);
        returned += 1;
        if let Ok(mut opened) = files.opened.lock() {
            opened.insert(file.path.clone());
        }
    }
    if returned == 0 {
        return Err(format!(
            "{} is not part of this change. The changed files are:\n{}",
            unknown.join(", "),
            diff.render_list()
        ));
    }
    if !unknown.is_empty() {
        let _ = write!(out, "\nNot part of this change: {}.", unknown.join(", "));
    }
    if !left_out.is_empty() {
        let _ = write!(
            out,
            "\nNot included, too long for one call; ask again: {}.",
            left_out.join(", ")
        );
    }
    Ok(out)
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
        let line_arg = |key: &str| {
            args.get(key)
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
        };
        let start = line_arg("start_line").unwrap_or(1);
        if start == 0 {
            return ToolOutput::error("start_line is 1-based; the first line is 1.");
        }
        let last = start + READ_FILE_MAX_LINES - 1;
        let end = match line_arg("end_line") {
            Some(end) if end < start => {
                return ToolOutput::error(format!(
                    "end_line {end} is before start_line {start}. Ask for start_line <= end_line, at most {READ_FILE_MAX_LINES} lines."
                ));
            }
            Some(end) => end.min(last),
            None => last,
        };
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
            description: "Lists the findings already on this pull request, with their comment ids, locations and text, and the drafts other reviewers of this review have queued. Call this before drafting a finding.".to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        let drafts: Vec<String> = match self.0.drafts.lock() {
            Ok(drafts) => drafts
                .iter()
                .map(|d| {
                    let what = match &d.kind {
                        DraftKind::Finding { .. } => "a new finding".to_owned(),
                        DraftKind::Rewrite { comment_id, .. } => {
                            format!("a rewrite of {comment_id}")
                        }
                        DraftKind::Withdrawal { comment_id, .. } => {
                            format!("withdrawing {comment_id}, because")
                        }
                    };
                    let mine = if d.lane == self.0.lane { ", yours" } else { "" };
                    format!(
                        "- draft {} at {}:{}{mine}: {what}\n  {}",
                        d.id, d.key.path, d.key.line, d.text
                    )
                })
                .collect(),
            Err(_) => return ToolOutput::error("drafts unavailable"),
        };
        let Ok(registry) = self.0.registry.lock() else {
            return ToolOutput::error("finding registry unavailable");
        };
        if registry.is_empty() && drafts.is_empty() {
            return ToolOutput::ok("No findings yet.");
        }
        let mut lines: Vec<String> = registry
            .iter()
            .map(|f| {
                let state = match (f.resolved, f.answered_by_person, f.in_diff) {
                    (true, _, _) => " [resolved]",
                    (_, true, _) => " [answered by a person; do not rewrite]",
                    (_, _, false) => " [outdated]",
                    _ => "",
                };
                let body = visible_text(&f.body);
                format!(
                    "- comment {} at {}:{}{state}\n  {}",
                    f.comment_id, f.key.path, f.key.line, body
                )
            })
            .collect();
        if !drafts.is_empty() {
            lines.push(
                "Drafts waiting for the fact-check, from every reviewer of this review (not posted yet):"
                    .to_owned(),
            );
            lines.extend(drafts);
        }
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
            description: "Drafts one finding as a comment on one line of the diff at the reviewed commit. Only for a real problem this change introduces. It is posted after the review if a fact-check confirms it. One finding per line; if the line already has one, improve it instead. Calling it again on a line with your own draft replaces that draft.".to_owned(),
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
        if let Err(reason) = ctx.files.diff.commentable(path, line, side) {
            return ToolOutput::error(format!(
                "Cannot post on {path}:{line}: {reason}. Use get_file_diff to see the numbered lines."
            ));
        }
        let key = FindingKey {
            path: path.to_owned(),
            line,
        };

        if let Some(refusal) = ctx.refuse_if_taken(&key).await {
            return refusal;
        }
        ctx.queue(DraftKind::Finding { side }, key, body).await
    }
}

/// `improve_finding`: rewrite an existing finding to explain it better.
pub struct ImproveFinding(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for ImproveFinding {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("improve_finding"),
            description: "Drafts a better text for an existing finding when you can explain the same problem better. It replaces the finding after the review if a fact-check confirms it. Never changes a finding a person has already answered.".to_owned(),
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
            return not_found(comment_id);
        };
        if existing.answered_by_person {
            return ToolOutput::error("A person has answered that finding; it stays as it is.");
        }
        let current = visible_text(&existing.body).to_owned();
        ctx.queue(
            DraftKind::Rewrite {
                comment_id: comment_id.to_owned(),
                current,
            },
            existing.key,
            body,
        )
        .await
    }
}

/// The refusal for a comment id that is not a finding on the pull request.
fn not_found(comment_id: &str) -> ToolOutput {
    if henk_domain::draft::DraftId::parse(comment_id).is_some() && comment_id.starts_with('d') {
        return ToolOutput::error(format!(
            "{comment_id} is a draft, not a comment yet; drafts are checked after the review and cannot be improved or withdrawn. To change your own, call post_finding again on its line."
        ));
    }
    ToolOutput::error(format!("No finding with comment id {comment_id}."))
}

/// The visible text of a comment: everything before the hidden marker.
pub(crate) fn visible_text(body: &str) -> &str {
    body.split("<!--").next().unwrap_or("").trim()
}

/// `withdraw_finding`: take back a finding that is wrong.
pub struct WithdrawFinding(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for WithdrawFinding {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("withdraw_finding"),
            description: "Drafts withdrawing an existing finding that is wrong. If a fact-check agrees after the review, its text is replaced by your reason and its thread resolved, so it no longer counts. Only for Henk's own findings that no person has answered.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "comment_id": {"type": "string", "description": "The comment id from list_existing_findings"},
                    "reason": {"type": "string", "description": "Why the finding is wrong, citing the code as path:line"}
                },
                "required": ["comment_id", "reason"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let ctx = &self.0;
        let (Some(comment_id), Some(reason)) =
            (arg_str(&args, "comment_id"), arg_str(&args, "reason"))
        else {
            return ToolOutput::error("comment_id and reason are required");
        };
        if let Some(error) = LaneContext::style_error(reason) {
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
            return not_found(comment_id);
        };
        if existing.answered_by_person {
            return ToolOutput::error(
                "A person has answered that finding; it stays as it is. Reply is for people.",
            );
        }
        if existing.resolved {
            return ToolOutput::error(format!("Finding {comment_id} is already resolved."));
        }
        let finding = visible_text(&existing.body).to_owned();
        ctx.queue(
            DraftKind::Withdrawal {
                comment_id: comment_id.to_owned(),
                finding,
            },
            existing.key,
            reason,
        )
        .await
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

    use henk_domain::allowlist::Platform;
    use henk_domain::diff::ReviewDiff;
    use henk_domain::review::LaneName;
    use henk_llm::ToolName;

    use super::*;

    pub(super) const DIFF: &str = "\
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

    pub(super) async fn context(diff: ReviewDiff) -> Arc<LaneContext> {
        let store = Arc::new(henk_store::SqliteStore::in_memory().unwrap());
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
            .await
            .unwrap();
        Arc::new(LaneContext {
            run,
            lane: LaneName::new("lane-a"),
            model: ModelId::parse("m").unwrap(),
            registry: Arc::new(Mutex::new(FindingRegistry::seeded(std::iter::empty()))),
            drafts: Arc::new(Mutex::new(DraftBook::new())),
            store,
            files: Arc::new(DiffFiles::new(Arc::new(diff))),
            workspace: None,
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
        let ctx = context(ReviewDiff::from_unified(DIFF)).await;
        let list = ListChangedFiles(Arc::clone(&ctx.files))
            .call(json!({}))
            .await;
        assert!(!list.is_error);
        assert!(list.content.contains("M src/a.rs (+1 -1)"));
        assert_eq!(ctx.unopened_files(), vec!["src/a.rs", "README.md"]);

        let file = GetFileDiff(Arc::clone(&ctx.files))
            .call(json!({"path": "src/a.rs"}))
            .await;
        assert!(!file.is_error, "{file:?}");
        assert!(
            file.content.contains("    2 |      |-    let x = 1;"),
            "{}",
            file.content
        );
        assert_eq!(ctx.unopened_files(), vec!["README.md"]);

        let missing = GetFileDiff(Arc::clone(&ctx.files))
            .call(json!({"path": "src/zzz.rs"}))
            .await;
        assert!(missing.is_error);
        assert!(missing.content.contains("README.md"));
    }

    #[tokio::test]
    async fn get_file_diff_reads_several_files_in_one_call() {
        let ctx = context(ReviewDiff::from_unified(DIFF)).await;
        let both = GetFileDiff(Arc::clone(&ctx.files))
            .call(json!({"paths": ["src/a.rs", "README.md"]}))
            .await;
        assert!(!both.is_error, "{both:?}");
        assert!(
            both.content.starts_with("== src/a.rs =="),
            "{}",
            both.content
        );
        assert!(both.content.contains("== README.md =="));
        assert!(ctx.unopened_files().is_empty(), "both count as opened");

        let mixed = GetFileDiff(Arc::clone(&ctx.files))
            .call(json!({"paths": ["src/zzz.rs", "README.md"]}))
            .await;
        assert!(!mixed.is_error);
        assert!(
            mixed
                .content
                .ends_with("Not part of this change: src/zzz.rs."),
            "{}",
            mixed.content
        );

        let none = GetFileDiff(Arc::clone(&ctx.files))
            .call(json!({"paths": ["src/zzz.rs"]}))
            .await;
        assert!(none.is_error);
        assert!(
            none.content.contains("README.md"),
            "the file list comes back"
        );

        assert!(
            GetFileDiff(Arc::clone(&ctx.files))
                .call(json!({}))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn a_batch_stops_at_the_cap_and_names_what_it_left_out() {
        let ctx = context(ReviewDiff::from_unified(DIFF)).await;
        let text = render_files(&ctx.files, &["src/a.rs", "README.md"], 10).unwrap();
        assert!(
            text.starts_with("== src/a.rs =="),
            "the first file always comes"
        );
        assert!(!text.contains("== README.md =="));
        assert!(
            text.ends_with("Not included, too long for one call; ask again: README.md."),
            "{text}"
        );
        assert_eq!(
            ctx.unopened_files(),
            vec!["README.md"],
            "left out is not opened"
        );
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
        let inverted = tool
            .call(json!({"path": "src/a.rs", "start_line": 3, "end_line": 1}))
            .await;
        assert!(inverted.is_error);
        assert!(
            inverted.content.contains("before start_line 3"),
            "{}",
            inverted.content
        );
        assert!(
            tool.call(json!({"path": "src/a.rs", "start_line": 0}))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn post_finding_refuses_lines_outside_the_diff_before_posting() {
        let ctx = context(ReviewDiff::from_unified(DIFF)).await;
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
        let inside = tool
            .call(json!({"path": "src/a.rs", "line": 2, "body": "Wrong."}))
            .await;
        assert!(!inside.is_error, "{}", inside.content);
        assert_eq!(ctx.drafts.lock().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod draft_tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::diff::ReviewDiff;
    use henk_domain::finding::Finding;

    use super::tests::DIFF;
    use super::*;

    /// lane-a, and lane-b on model `n` sharing its registry and drafts.
    async fn two_lanes() -> (Arc<LaneContext>, Arc<LaneContext>) {
        let a = super::tests::context(ReviewDiff::from_unified(DIFF)).await;
        let b = Arc::new(LaneContext {
            run: a.run.clone(),
            lane: LaneName::new("lane-b"),
            model: ModelId::parse("n").unwrap(),
            registry: Arc::clone(&a.registry),
            drafts: Arc::clone(&a.drafts),
            store: Arc::clone(&a.store),
            files: Arc::clone(&a.files),
            workspace: None,
        });
        (a, b)
    }

    fn post(body: &str) -> Value {
        json!({"path": "src/a.rs", "line": 2, "body": body})
    }

    async fn actions(ctx: &LaneContext) -> Vec<String> {
        ctx.store
            .findings(&ctx.run)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.action)
            .collect()
    }

    #[tokio::test]
    async fn a_post_queues_a_draft_on_the_run_and_writes_nothing() {
        let (a, _) = two_lanes().await;
        let out = PostFinding(Arc::clone(&a))
            .call(post("x is never set."))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.starts_with("Queued as draft d1."),
            "{}",
            out.content
        );
        assert!(out.content.contains("you will not hear the verdict"));
        let book = a.drafts.lock().unwrap().clone();
        let draft = book.iter().next().unwrap();
        assert_eq!((draft.lane.as_str(), draft.model.as_str()), ("lane-a", "m"));
        assert_eq!(draft.text, "x is never set.");
        let stored = a.store.drafts(&a.run).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            (
                stored[0].draft.as_str(),
                stored[0].kind.as_str(),
                stored[0].line
            ),
            ("d1", "finding", 2)
        );
        assert!(stored[0].decision.is_none());
        assert!(
            actions(&a).await.is_empty(),
            "nothing written, nothing refused"
        );
    }

    #[tokio::test]
    async fn a_line_holds_one_draft_and_a_lane_may_replace_its_own() {
        let (a, b) = two_lanes().await;
        PostFinding(Arc::clone(&a))
            .call(post("x is never set."))
            .await;
        let other = PostFinding(Arc::clone(&b)).call(post("x is wrong.")).await;
        assert!(other.is_error);
        assert!(
            other
                .content
                .contains("already has draft d1 by another reviewer"),
            "{}",
            other.content
        );
        assert_eq!(actions(&b).await, ["refused"]);
        let again = PostFinding(Arc::clone(&a))
            .call(post("x is never set on the error path."))
            .await;
        assert!(
            again.content.starts_with("Replaced your draft d1"),
            "{}",
            again.content
        );
        assert_eq!(a.drafts.lock().unwrap().len(), 1);
        let stored = a.store.drafts(&a.run).await.unwrap();
        assert_eq!(stored[0].body, "x is never set on the error path.");
    }

    /// Two lanes that post on one line at the same moment draft once: the
    /// line is taken under the drafts' lock, and nothing is posted before
    /// the lanes are done (#49).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_lanes_on_one_line_at_once_draft_once() {
        for _ in 0..50 {
            let (a, b) = two_lanes().await;
            let start = Arc::new(tokio::sync::Barrier::new(2));
            let lane = |ctx: &Arc<LaneContext>, body: &'static str| {
                let (ctx, start) = (Arc::clone(ctx), Arc::clone(&start));
                tokio::spawn(async move {
                    start.wait().await;
                    PostFinding(ctx).call(post(body)).await
                })
            };
            let (first, second) = (lane(&a, "x is never set."), lane(&b, "x is wrong."));
            let outputs = [first.await.unwrap(), second.await.unwrap()];
            let refused: Vec<_> = outputs.iter().filter(|o| o.is_error).collect();
            assert_eq!(refused.len(), 1, "one drafts, one is refused");
            assert!(
                refused[0].content.contains("by another reviewer"),
                "{}",
                refused[0].content
            );
            assert_eq!(a.drafts.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn other_lanes_see_waiting_drafts_with_the_findings() {
        let (a, b) = two_lanes().await;
        let empty = ListExistingFindings(Arc::clone(&b)).call(json!({})).await;
        assert_eq!(empty.content, "No findings yet.");
        PostFinding(Arc::clone(&a))
            .call(post("x is never set."))
            .await;
        let listed = ListExistingFindings(Arc::clone(&b)).call(json!({})).await;
        assert!(
            listed.content.contains("Drafts waiting for the fact-check"),
            "{}",
            listed.content
        );
        assert!(
            listed
                .content
                .contains("- draft d1 at src/a.rs:2: a new finding\n  x is never set."),
            "{}",
            listed.content
        );
        let own = ListExistingFindings(Arc::clone(&a)).call(json!({})).await;
        assert!(own.content.contains("src/a.rs:2, yours"), "{}", own.content);
    }

    fn seed(ctx: &LaneContext, answered: bool, resolved: bool) {
        ctx.registry.lock().unwrap().record(Finding {
            key: FindingKey {
                path: "src/a.rs".into(),
                line: 2,
            },
            comment_id: "c9".into(),
            body: "x is never set.<!-- marker -->".into(),
            lane: None,
            answered_by_person: answered,
            resolved,
            in_diff: true,
        });
    }

    #[tokio::test]
    async fn rewrites_and_withdrawals_are_drafts_about_their_comment() {
        let (a, b) = two_lanes().await;
        seed(&a, false, false);
        let rewrite = ImproveFinding(Arc::clone(&a))
            .call(json!({"comment_id": "c9", "body": "x is never set on line 2."}))
            .await;
        assert!(
            rewrite.content.starts_with("Queued as draft d1."),
            "{}",
            rewrite.content
        );
        let draft = a
            .drafts
            .lock()
            .unwrap()
            .get(henk_domain::draft::DraftId::parse("d1").unwrap())
            .cloned()
            .unwrap();
        assert_eq!(
            draft.kind,
            DraftKind::Rewrite {
                comment_id: "c9".into(),
                current: "x is never set.".into()
            }
        );
        let withdraw = WithdrawFinding(Arc::clone(&b))
            .call(json!({"comment_id": "c9", "reason": "src/a.rs:2 sets x."}))
            .await;
        assert!(withdraw.is_error, "one draft per comment");
        let new = PostFinding(Arc::clone(&b)).call(post("Another.")).await;
        assert!(
            new.content.contains("already has finding c9"),
            "{}",
            new.content
        );
        assert_eq!(a.store.drafts(&a.run).await.unwrap()[0].target, "c9");
    }

    #[tokio::test]
    async fn answered_resolved_and_draft_ids_are_refused() {
        let (a, _) = two_lanes().await;
        seed(&a, true, false);
        let answered = WithdrawFinding(Arc::clone(&a))
            .call(json!({"comment_id": "c9", "reason": "Wrong."}))
            .await;
        assert!(
            answered.content.contains("A person has answered"),
            "{}",
            answered.content
        );
        let draft = ImproveFinding(Arc::clone(&a))
            .call(json!({"comment_id": "d1", "body": "Better."}))
            .await;
        assert!(
            draft.content.contains("d1 is a draft, not a comment yet"),
            "{}",
            draft.content
        );
        let (b, _) = two_lanes().await;
        seed(&b, false, true);
        let resolved = WithdrawFinding(Arc::clone(&b))
            .call(json!({"comment_id": "c9", "reason": "Wrong."}))
            .await;
        assert!(
            resolved.content.contains("already resolved"),
            "{}",
            resolved.content
        );
        assert!(a.drafts.lock().unwrap().is_empty() && b.drafts.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod continuation_tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use henk_agent::{EndReason, Ending};
    use henk_domain::diff::ReviewDiff;

    use super::tests::context;
    use super::*;

    fn ending(reason: EndReason) -> Ending<'static> {
        Ending {
            reason,
            turn: 3,
            messages: &[],
        }
    }

    #[tokio::test]
    async fn nudges_once_for_coverage_and_once_for_an_output_cap() {
        let ctx = context(ReviewDiff::from_unified(super::tests::DIFF)).await;
        let nudge = lane_continuation(Arc::clone(&ctx));
        let first = nudge(&ending(EndReason::EndTurn)).unwrap();
        assert!(
            first.contains("2 changed file(s): src/a.rs, README.md"),
            "{first}"
        );
        assert_eq!(nudge(&ending(EndReason::EndTurn)), None, "only once");
        let cap = nudge(&ending(EndReason::OutputCap)).unwrap();
        assert!(cap.contains("cut off"));
        assert_eq!(nudge(&ending(EndReason::OutputCap)), None);
    }

    #[tokio::test]
    async fn no_coverage_nudge_when_every_file_was_opened() {
        let ctx = context(ReviewDiff::from_unified(super::tests::DIFF)).await;
        for path in ["src/a.rs", "README.md"] {
            GetFileDiff(Arc::clone(&ctx.files))
                .call(json!({"path": path}))
                .await;
        }
        assert_eq!(lane_continuation(ctx)(&ending(EndReason::EndTurn)), None);
    }
}
