//! The tools a review lane gets besides the read-only MCP tools:
//! listing, posting and improving findings, with every rule of §3.2 and
//! §8.5 enforced here rather than in the prompt.

use std::sync::{Arc, Mutex};

use henk_agent::{Tool, ToolOutput};
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
