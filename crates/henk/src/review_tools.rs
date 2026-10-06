//! The tools a review lane gets besides the read-only MCP tools:
//! listing, posting, improving and withdrawing findings, with every rule of
//! §3.2 and §8.5 enforced here rather than in the prompt. When a fact-check
//! is configured, every write passes it first.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{Continuation, EndReason, Ending, Tool, ToolOutput};
use henk_domain::diff::ReviewDiff;
use henk_domain::finding::{Claim, Finding, FindingKey, FindingRegistry};
use henk_domain::marker::{Marker, MarkerKind, ModelId, Withdrawal};
use henk_domain::review::{CommitSha, LaneName};
use henk_domain::run::RunId;
use henk_domain::text::style_violations;
use henk_llm::{ToolDef, ToolName};
use henk_platform::{DiffSide, PlatformWriter, ReviewTarget};
use henk_store::{FindingAction, RunStore};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::fact_check::{CheckKind, CheckRequest, CheckVerdict, FactCheck};

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
    /// The pull/merge request.
    pub target: ReviewTarget,
    /// The reviewed commit.
    pub commit: CommitSha,
    /// Findings known on the target, shared by all lanes of the review.
    pub registry: Arc<Mutex<FindingRegistry>>,
    /// Where comments go.
    pub writer: Arc<dyn PlatformWriter>,
    /// Run records.
    pub store: Arc<dyn RunStore>,
    /// The diff of the review. Findings are checked against it before
    /// anything is posted.
    pub files: Arc<DiffFiles>,
    /// The second model every write passes, when configured (§3.2).
    pub fact_check: Option<Arc<dyn FactCheck>>,
    /// Fact-check rejections per line, for this lane.
    pub rejections: Mutex<BTreeMap<FindingKey, u32>>,
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

/// What the fact-check decided for one write.
enum Gate {
    /// Go ahead.
    Pass {
        /// The model that confirmed the write, for its marker.
        checked_by: Option<ModelId>,
        /// Why no check could be made, when none was.
        unchecked: Option<String>,
    },
    /// Do not write; tell the lane this.
    Stop(ToolOutput),
}

/// How often a line may be rejected before the lane is told to stop trying.
const MAX_REJECTIONS: u32 = 2;

impl LaneContext {
    /// Runs the fact-check for one write. `what` completes "Not ...":
    /// "posted", "updated", "withdrawn".
    async fn gate(
        &self,
        kind: CheckKind,
        key: &FindingKey,
        side: DiffSide,
        text: &str,
        comment_id: Option<&str>,
        what: &str,
    ) -> Gate {
        let Some(checker) = &self.fact_check else {
            return Gate::Pass {
                checked_by: None,
                unchecked: None,
            };
        };
        let earlier = self
            .rejections
            .lock()
            .map_or(0, |r| r.get(key).copied().unwrap_or(0));
        if earlier >= MAX_REJECTIONS {
            return Gate::Stop(ToolOutput::error(format!(
                "Not {what}: the fact-check has rejected claims on {}:{} {earlier} times. Do not try this line again; move on.",
                key.path, key.line
            )));
        }
        let request = CheckRequest {
            lane: self.lane.clone(),
            lane_model: self.model.clone(),
            kind,
            path: key.path.clone(),
            line: key.line,
            side,
            text: text.to_owned(),
        };
        match checker.check(&request).await {
            CheckVerdict::Confirmed { by, .. } => Gate::Pass {
                checked_by: Some(by),
                unchecked: None,
            },
            CheckVerdict::Unavailable { why } => Gate::Pass {
                checked_by: None,
                unchecked: Some(why),
            },
            CheckVerdict::Rejected { reason, .. } => {
                let count = self.rejections.lock().map_or(MAX_REJECTIONS, |mut r| {
                    let count = r.entry(key.clone()).or_insert(0);
                    *count += 1;
                    *count
                });
                let _ = self
                    .store
                    .record_finding(
                        &self.run,
                        self.lane.as_str(),
                        &key.path,
                        key.line,
                        comment_id.unwrap_or(""),
                        FindingAction::Rejected,
                    )
                    .await;
                let next = if count >= MAX_REJECTIONS {
                    "It has now been rejected twice: do not try this line again; move on."
                } else {
                    "If the problem still holds, correct what the reason says is wrong and try again; otherwise move on."
                };
                Gate::Stop(ToolOutput::error(format!(
                    "Not {what}: a fact-check rejected it. Reason: {reason}\n{next}"
                )))
            }
        }
    }

    /// The refusal when `key` already has a finding: improve it instead.
    ///
    /// Claim under the lock, then release it before any I/O: a std mutex
    /// guard must not live across an await. Two lanes racing between the
    /// claim and the post is rare, and the registry stays consistent because
    /// the post result is recorded under the lock again.
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

    /// Records a write that went out unchecked and says so to the lane.
    async fn unverified(&self, key: &FindingKey, comment_id: &str, why: Option<&str>) -> String {
        let Some(why) = why else {
            return String::new();
        };
        let _ = self
            .store
            .record_finding(
                &self.run,
                self.lane.as_str(),
                &key.path,
                key.line,
                comment_id,
                FindingAction::Unverified,
            )
            .await;
        format!(" The fact-check could not run ({why}), so it went out unchecked.")
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
                "Your answer was cut off at the output limit. Post each finding you are sure of with post_finding, one call per finding, then end your turn.".to_owned(),
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
    /// The marker of a finding this lane writes, naming the model that
    /// fact-checked it, when one did.
    fn marker(&self, checked_by: Option<ModelId>) -> Marker {
        Marker {
            run: self.run.clone(),
            model: self.model.clone(),
            requested_by: None,
            kind: Some(MarkerKind::Finding),
            checked_by,
            withdrawn: None,
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
                let body = visible_text(&f.body);
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

        let (checked_by, unchecked) = match ctx
            .gate(CheckKind::NewFinding, &key, side, body, None, "posted")
            .await
        {
            Gate::Pass {
                checked_by,
                unchecked,
            } => (checked_by, unchecked),
            Gate::Stop(output) => return output,
        };

        let full_body = ctx.marker(checked_by).attach(body);
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
                let _ = ctx
                    .store
                    .event(
                        &ctx.run,
                        "warn",
                        &format!(
                            "{}: could not post a finding on {path}:{line}: {error}",
                            ctx.lane
                        ),
                    )
                    .await;
                return ToolOutput::error(format!(
                    "Could not post on {path}:{line}: {error}. If the line is not part of the diff, pick a line that is."
                ));
            }
        };
        if let Ok(mut registry) = ctx.registry.lock() {
            registry.record(Finding {
                key: key.clone(),
                comment_id: posted.id.clone(),
                body: full_body,
                lane: Some(ctx.lane.clone()),
                answered_by_person: false,
                resolved: false,
                in_diff: true,
            });
        }
        let _ = ctx
            .store
            .record_finding(
                &ctx.run,
                ctx.lane.as_str(),
                path,
                line,
                &posted.id,
                FindingAction::Posted,
            )
            .await;
        info!(lane = %ctx.lane, path, line, comment = %posted.id, "finding posted");
        let note = ctx.unverified(&key, &posted.id, unchecked.as_deref()).await;
        ToolOutput::ok(format!("Posted as comment {}.{note}", posted.id))
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
        let current = visible_text(&existing.body).to_owned();
        let (checked_by, unchecked) = match ctx
            .gate(
                CheckKind::Rewrite { current },
                &existing.key,
                DiffSide::Right,
                body,
                Some(comment_id),
                "updated",
            )
            .await
        {
            Gate::Pass {
                checked_by,
                unchecked,
            } => (checked_by, unchecked),
            Gate::Stop(output) => return output,
        };
        let full_body = ctx.marker(checked_by).attach(body);
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
        let _ = ctx
            .store
            .record_finding(
                &ctx.run,
                ctx.lane.as_str(),
                &existing.key.path,
                existing.key.line,
                comment_id,
                FindingAction::Improved,
            )
            .await;
        let note = ctx
            .unverified(&existing.key, comment_id, unchecked.as_deref())
            .await;
        ToolOutput::ok(format!("Updated comment {comment_id}.{note}"))
    }
}

/// The visible text of a comment: everything before the hidden marker.
fn visible_text(body: &str) -> &str {
    body.split("<!--").next().unwrap_or("").trim()
}

/// `withdraw_finding`: take back a finding that is wrong.
pub struct WithdrawFinding(pub Arc<LaneContext>);

#[async_trait::async_trait]
impl Tool for WithdrawFinding {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("withdraw_finding"),
            description: "Withdraws an existing finding that is wrong: its text is replaced by your reason and its thread is resolved, so it no longer counts. Only for Henk's own findings that no person has answered.".to_owned(),
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
            return ToolOutput::error(format!("No finding with comment id {comment_id}."));
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
        let (checked_by, unchecked) = match ctx
            .gate(
                CheckKind::Withdrawal { finding },
                &existing.key,
                DiffSide::Right,
                reason,
                Some(comment_id),
                "withdrawn",
            )
            .await
        {
            Gate::Pass {
                checked_by,
                unchecked,
            } => (checked_by, unchecked),
            Gate::Stop(output) => return output,
        };
        // The finding keeps who wrote it (§8.6); the withdrawal is added.
        let original = Marker::parse(&existing.body).unwrap_or_else(|| ctx.marker(None));
        let full_body = Marker {
            withdrawn: Some(Withdrawal {
                run: ctx.run.clone(),
                model: ctx.model.clone(),
                checked_by,
            }),
            ..original
        }
        .attach(&format!("Withdrawn. {reason}"));
        if let Err(error) = ctx
            .writer
            .update_finding(&ctx.target, comment_id, &full_body)
            .await
        {
            return ToolOutput::error(format!("Could not update comment {comment_id}: {error}"));
        }
        if let Err(error) = ctx.writer.resolve_finding(&ctx.target, comment_id).await {
            // The text already says it is withdrawn; an open thread only
            // means it still counts until someone resolves it.
            warn!(%error, comment = comment_id, "could not resolve a withdrawn finding");
            let _ = ctx
                .store
                .event(
                    &ctx.run,
                    "warn",
                    &format!(
                        "{}: withdrew {comment_id} but could not resolve its thread: {error}",
                        ctx.lane
                    ),
                )
                .await;
        }
        if let Ok(mut registry) = ctx.registry.lock() {
            registry.withdraw(&existing.key, full_body);
        }
        let _ = ctx
            .store
            .record_finding(
                &ctx.run,
                ctx.lane.as_str(),
                &existing.key.path,
                existing.key.line,
                comment_id,
                FindingAction::Withdrawn,
            )
            .await;
        info!(lane = %ctx.lane, comment = comment_id, "finding withdrawn");
        let note = ctx
            .unverified(&existing.key, comment_id, unchecked.as_deref())
            .await;
        ToolOutput::ok(format!("Withdrew comment {comment_id}.{note}"))
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
            target: ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 7,
            },
            commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
            registry: Arc::new(Mutex::new(FindingRegistry::seeded(std::iter::empty()))),
            writer: Arc::new(FakeWriter::default()),
            store,
            files: Arc::new(DiffFiles::new(Arc::new(diff))),
            fact_check: None,
            rejections: Mutex::new(BTreeMap::new()),
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
        assert_eq!(ctx.store.events(&ctx.run).await.unwrap().len(), 1);
    }
}

#[cfg(test)]
mod gate_tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::collections::VecDeque;

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::diff::ReviewDiff;

    use super::tests::DIFF;
    use super::*;
    use crate::fact_check::{CheckRequest, CheckVerdict, FactCheck};
    use crate::listeners::testing::FakeWriter;

    /// Answers each check with the next verdict and keeps the requests.
    struct Fixed {
        verdicts: Mutex<VecDeque<CheckVerdict>>,
        seen: Mutex<Vec<CheckRequest>>,
    }

    #[async_trait::async_trait]
    impl FactCheck for Fixed {
        async fn check(&self, request: &CheckRequest) -> CheckVerdict {
            self.seen.lock().unwrap().push(request.clone());
            self.verdicts.lock().unwrap().pop_front().unwrap()
        }
    }

    fn by() -> ModelId {
        ModelId::parse("opus").unwrap()
    }

    fn rejected(reason: &str) -> CheckVerdict {
        CheckVerdict::Rejected {
            by: by(),
            reason: reason.into(),
        }
    }

    fn confirmed() -> CheckVerdict {
        CheckVerdict::Confirmed {
            by: by(),
            reason: "holds".into(),
        }
    }

    async fn setup(
        verdicts: impl IntoIterator<Item = CheckVerdict>,
    ) -> (Arc<LaneContext>, Arc<FakeWriter>, Arc<Fixed>) {
        let base = super::tests::context(ReviewDiff::from_unified(DIFF)).await;
        let writer = Arc::new(FakeWriter {
            accept_posts: true,
            ..FakeWriter::default()
        });
        let checker = Arc::new(Fixed {
            verdicts: Mutex::new(verdicts.into_iter().collect()),
            seen: Mutex::new(Vec::new()),
        });
        let ctx = Arc::new(LaneContext {
            run: base.run.clone(),
            lane: base.lane.clone(),
            model: base.model.clone(),
            target: ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 7,
            },
            commit: base.commit.clone(),
            registry: Arc::clone(&base.registry),
            writer: Arc::clone(&writer) as Arc<dyn PlatformWriter>,
            store: Arc::clone(&base.store),
            files: Arc::clone(&base.files),
            fact_check: Some(Arc::clone(&checker) as Arc<dyn FactCheck>),
            rejections: Mutex::new(BTreeMap::new()),
            workspace: None,
        });
        (ctx, writer, checker)
    }

    fn post() -> Value {
        json!({"path": "src/a.rs", "line": 2, "body": "x is never set."})
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
    async fn a_confirmed_finding_is_posted() {
        let (ctx, writer, checker) = setup([confirmed()]).await;
        let out = PostFinding(Arc::clone(&ctx)).call(post()).await;
        assert!(!out.is_error, "{out:?}");
        let posts = writer.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        let marker = Marker::parse(&posts[0].2).unwrap();
        assert_eq!(marker.checked_by.unwrap().as_str(), "opus");
        let seen = checker.seen.lock().unwrap();
        assert_eq!(seen[0].text, "x is never set.");
        assert_eq!(seen[0].lane_model.as_str(), "m");
    }

    #[tokio::test]
    async fn a_rejected_finding_is_not_posted_and_the_lane_hears_why_and_twice_is_final() {
        let (ctx, writer, _) =
            setup([rejected("src/a.rs:2 sets x."), rejected("Still set.")]).await;
        let tool = PostFinding(Arc::clone(&ctx));
        let first = tool.call(post()).await;
        assert!(first.is_error);
        assert!(
            first.content.contains("Reason: src/a.rs:2 sets x."),
            "{}",
            first.content
        );
        assert!(first.content.contains("try again"), "{}", first.content);
        let second = tool.call(post()).await;
        assert!(
            second.content.contains("rejected twice"),
            "{}",
            second.content
        );
        let third = tool.call(post()).await;
        assert!(
            third.content.contains("Do not try this line again"),
            "{}",
            third.content
        );
        assert!(writer.posts.lock().unwrap().is_empty());
        assert_eq!(actions(&ctx).await, vec!["rejected", "rejected"]);
    }

    #[tokio::test]
    async fn an_unavailable_check_posts_unchecked_and_says_so() {
        let (ctx, writer, _) = setup([CheckVerdict::Unavailable {
            why: "no verdict from opus".into(),
        }])
        .await;
        let out = PostFinding(Arc::clone(&ctx)).call(post()).await;
        assert!(!out.is_error);
        assert!(
            out.content.contains("went out unchecked"),
            "{}",
            out.content
        );
        assert_eq!(actions(&ctx).await, vec!["posted", "unverified"]);
        let posts = writer.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(Marker::parse(&posts[0].2).unwrap().checked_by, None);
    }

    #[tokio::test]
    async fn a_confirmed_withdrawal_rewrites_resolves_and_stops_counting() {
        let (ctx, writer, checker) = setup([confirmed(), confirmed()]).await;
        PostFinding(Arc::clone(&ctx)).call(post()).await;
        assert_eq!(ctx.registry.lock().unwrap().open_count(), 1);
        let out = WithdrawFinding(Arc::clone(&ctx))
            .call(json!({"comment_id": "c1", "reason": "src/a.rs:2 does set x."}))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(actions(&ctx).await, vec!["posted", "withdrawn"]);
        let updates = writer.updates.lock().unwrap();
        assert!(
            updates[0]
                .1
                .starts_with("Withdrawn. src/a.rs:2 does set x."),
            "{}",
            updates[0].1
        );
        assert_eq!(*writer.resolved.lock().unwrap(), vec!["c1".to_owned()]);
        assert_eq!(ctx.registry.lock().unwrap().open_count(), 0);
        let seen = checker.seen.lock().unwrap();
        assert!(
            matches!(&seen[1].kind, CheckKind::Withdrawal { finding } if finding == "x is never set.")
        );
    }

    #[tokio::test]
    async fn a_withdrawn_finding_keeps_its_author_and_names_who_withdrew_it() {
        let (ctx, writer, _) = setup([confirmed()]).await;
        // A finding an earlier run posted, with another model.
        let original = Marker {
            run: RunId::parse("r-0").unwrap(),
            model: ModelId::parse("orig").unwrap(),
            checked_by: None,
            requested_by: None,
            kind: Some(MarkerKind::Finding),
            withdrawn: None,
        };
        ctx.registry.lock().unwrap().record(Finding {
            key: FindingKey {
                path: "src/a.rs".into(),
                line: 2,
            },
            comment_id: "c9".into(),
            body: original.attach("x is never set."),
            lane: None,
            answered_by_person: false,
            resolved: false,
            in_diff: true,
        });
        let out = WithdrawFinding(Arc::clone(&ctx))
            .call(json!({"comment_id": "c9", "reason": "src/a.rs:2 sets x."}))
            .await;
        assert!(!out.is_error, "{out:?}");
        let updates = writer.updates.lock().unwrap();
        let body = &updates[0].1;
        let marker = Marker::parse(body).unwrap();
        assert_eq!(marker.run.as_str(), "r-0", "the original run stays");
        assert_eq!(marker.model.as_str(), "orig", "the original model stays");
        let withdrawal = marker.withdrawn.unwrap();
        assert_eq!(withdrawal.run, ctx.run);
        assert_eq!(withdrawal.model, ctx.model);
        assert_eq!(withdrawal.checked_by.unwrap().as_str(), "opus");
        assert!(body.starts_with("Withdrawn. src/a.rs:2 sets x."), "{body}");
        assert!(
            body.contains("was withdrawn by Meneer Henk"),
            "the withdrawal note"
        );
        assert!(!body.contains("address it"), "not the finding note");
    }

    #[tokio::test]
    async fn a_rejected_rewrite_leaves_the_finding_alone() {
        let (ctx, writer, _) = setup([confirmed(), rejected("The rewrite is wrong.")]).await;
        PostFinding(Arc::clone(&ctx)).call(post()).await;
        let out = ImproveFinding(Arc::clone(&ctx))
            .call(json!({"comment_id": "c1", "body": "Nothing is wrong here."}))
            .await;
        assert!(out.is_error);
        assert!(writer.updates.lock().unwrap().is_empty());
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
