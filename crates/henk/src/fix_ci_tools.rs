//! The tool an executor ends a fix-CI run with (#583): it proposes the fix,
//! or says the failing checks are not about the code. Nothing here reaches
//! the platform.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{Continuation, Tool, ToolOutput};
use henk_domain::commit::CommitProposal;
use henk_domain::text::style_violations;
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

use crate::commit_tools::{check_commits, commits_schema, parse_commits};
use crate::workspace::Workspace;

/// Nudges the executor gets before a run without a proposal ends.
const NUDGES: u32 = 2;

/// What the executor found the failure to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// The code is at fault, and the workspace holds the fix.
    Code,
    /// The failure is not about the code; what the logs show.
    NotCode(String),
}

/// The fix the executor proposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixProposal {
    /// What failed, what changed, and which checks ran.
    pub summary: String,
    /// What the failure was.
    pub cause: Cause,
    /// How to commit the changes (#400); one commit when `None`.
    pub commits: Option<Vec<CommitProposal>>,
}

/// Where the proposal waits for Henk's code.
pub type FixProposed = Arc<Mutex<Option<FixProposal>>>;

/// `propose_fix`: the executor is done.
pub struct ProposeFix {
    /// Where the proposal waits.
    pub proposed: FixProposed,
    /// The workspace the changes are in, to check the commits against.
    pub workspace: Arc<dyn Workspace>,
}

#[async_trait::async_trait]
impl Tool for ProposeFix {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("propose_fix")
                .unwrap_or_else(|_| unreachable!("a constant tool name")),
            description: "Ends your work. Call it once, last. cause is code when the failing checks are the code's fault and you fixed it: Henk commits your changes to the pull request's branch and pushes them; he runs no checks of his own, and the pull request's CI decides. summary: what failed, what you changed, and which checks you ran and how they ended, in a few sentences. commits: how Henk commits your changes, in the repository's commit convention, one commit per coherent change; without it, one commit. cause is not_code when the failure is not about the code, such as a lost runner, an unavailable service or a rate limit: change nothing, and say in reason what the logs show; Henk pushes nothing and tells the pull request why.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cause": {"type": "string", "enum": ["code", "not_code"]},
                    "summary": {"type": "string"},
                    "reason": {"type": "string", "description": "With not_code: what the logs show"},
                    "commits": commits_schema()
                },
                "required": ["cause", "summary"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_owned()
        };
        let (summary, reason) = (text("summary"), text("reason"));
        if summary.is_empty() {
            return ToolOutput::error("summary is required");
        }
        if [&summary, &reason]
            .iter()
            .any(|t| !style_violations(t).is_empty())
        {
            return ToolOutput::error(
                "The text breaks the style rules (no emoji, no em-dash). Rewrite it.",
            );
        }
        let has_commits = !matches!(args.get("commits"), None | Some(Value::Null));
        let cause = match text("cause").as_str() {
            "code" => Cause::Code,
            "not_code" => {
                if reason.is_empty() {
                    return ToolOutput::error("reason is required with cause not_code");
                }
                if has_commits {
                    return ToolOutput::error(
                        "commits belong to a fix: leave them out with cause not_code",
                    );
                }
                Cause::NotCode(reason)
            }
            _ => return ToolOutput::error("cause must be code or not_code"),
        };
        let commits = match args.get("commits") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let checked = match parse_commits(value) {
                    Ok(commits) => check_commits(&*self.workspace, &commits)
                        .await
                        .map(|()| commits),
                    Err(why) => Err(why),
                };
                match checked {
                    Ok(commits) => Some(commits),
                    Err(why) => {
                        return ToolOutput::error(format!(
                            "Not proposed: {why}. Fix the commits, or leave them out for one commit."
                        ));
                    }
                }
            }
        };
        let Ok(mut proposed) = self.proposed.lock() else {
            return ToolOutput::error("proposal unavailable");
        };
        *proposed = Some(FixProposal {
            summary,
            cause,
            commits,
        });
        ToolOutput::ok("Proposed. Henk takes it from here. End your turn now.")
    }
}

/// The executor's nudge: propose the fix, or say why there is none, at most
/// [`NUDGES`] times.
#[must_use]
pub fn fix_continuation(proposed: FixProposed) -> Continuation {
    let nudged = AtomicU32::new(0);
    Box::new(move |_| {
        let done = proposed.lock().is_ok_and(|p| p.is_some());
        if done || nudged.fetch_add(1, Ordering::SeqCst) >= NUDGES {
            return None;
        }
        Some(
            "End with propose_fix: cause code and a summary of what you changed, or cause not_code and what the logs show. Only then end your turn."
                .to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use henk_domain::path::WorkspacePath;

    use super::*;
    use crate::edit_tools::tests::workspace;

    async fn tool(name: &str) -> (ProposeFix, FixProposed, Arc<dyn Workspace>) {
        let (_provider, ctx) = workspace(name).await;
        let proposed: FixProposed = Arc::default();
        let tool = ProposeFix {
            proposed: Arc::clone(&proposed),
            workspace: Arc::clone(&ctx.workspace),
        };
        (tool, proposed, Arc::clone(&ctx.workspace))
    }

    #[tokio::test]
    async fn a_fix_needs_a_cause_and_a_summary_in_style() {
        let (tool, proposed, _workspace) = tool("henk-fix-style").await;
        for args in [
            json!({"summary": "x"}),
            json!({"cause": "bug", "summary": "x"}),
            json!({"cause": "code"}),
            json!({"cause": "code", "summary": "Done \u{1f680}"}),
        ] {
            assert!(tool.call(args.clone()).await.is_error, "{args}");
        }
        assert!(proposed.lock().unwrap().is_none());
        let out = tool
            .call(json!({"cause": "code", "summary": "The export test now expects the header."}))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            *proposed.lock().unwrap(),
            Some(FixProposal {
                summary: "The export test now expects the header.".to_owned(),
                cause: Cause::Code,
                commits: None,
            })
        );
    }

    #[tokio::test]
    async fn not_code_needs_its_reason_and_takes_no_commits() {
        let (tool, proposed, _workspace) = tool("henk-fix-notcode").await;
        let no_reason = tool
            .call(json!({"cause": "not_code", "summary": "The runner was lost."}))
            .await;
        assert!(no_reason.is_error, "{no_reason:?}");
        let commits = tool
            .call(
                json!({"cause": "not_code", "summary": "s", "reason": "r", "commits": [
                    {"files": ["a.rs"], "subject": "fix: x"}
                ]}),
            )
            .await;
        assert!(commits.is_error, "{commits:?}");
        assert!(proposed.lock().unwrap().is_none());
        let out = tool
            .call(json!({
                "cause": "not_code",
                "summary": "The test job never started.",
                "reason": "The log ends with: the runner has received a shutdown signal."
            }))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            proposed.lock().unwrap().clone().unwrap().cause,
            Cause::NotCode(
                "The log ends with: the runner has received a shutdown signal.".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn proposed_commits_must_hold_every_change_once() {
        let (tool, proposed, workspace) = tool("henk-fix-commits").await;
        for path in ["src/a.rs", "src/csv.rs"] {
            workspace
                .write(
                    &WorkspacePath::parse(path).unwrap(),
                    b"pub fn export() {}\n",
                )
                .await
                .unwrap();
        }
        let missing = tool
            .call(json!({"cause": "code", "summary": "s", "commits": [
                {"files": ["src/csv.rs"], "subject": "fix: export"}
            ]}))
            .await;
        assert!(
            missing.is_error && missing.content.contains("in no commit: src/a.rs"),
            "{missing:?}"
        );
        assert!(proposed.lock().unwrap().is_none());
        let out = tool
            .call(json!({"cause": "code", "summary": "s", "commits": [
                {"files": ["src/csv.rs"], "subject": "fix: export"},
                {"files": ["src/a.rs"], "subject": "test: call the exporter"}
            ]}))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            proposed
                .lock()
                .unwrap()
                .clone()
                .unwrap()
                .commits
                .unwrap()
                .len(),
            2
        );
    }
}
