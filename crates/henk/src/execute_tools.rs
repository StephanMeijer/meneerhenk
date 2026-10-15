//! The tool an executor ends an execute run with (#329): it proposes the
//! pull request, and Henk's code checks, commits, pushes and opens it.
//! Nothing here reaches the platform.

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

/// The pull request the executor proposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Its title; the issue's when the executor gave none.
    pub title: Option<String>,
    /// What changed.
    pub summary: String,
    /// What was left out, and why.
    pub left_out: String,
    /// How to commit the changes (#400); one commit when `None`.
    pub commits: Option<Vec<CommitProposal>>,
    /// How the requirements review ended (#535), for the pull request's
    /// body. Henk's code fills it in after the executor proposed; the
    /// executor does not write it, and it is empty when there was no review.
    pub requirements: String,
}

/// Where the proposal waits for Henk's code.
pub type Proposed = Arc<Mutex<Option<Proposal>>>;

/// `propose_pull_request`: the executor is done.
pub struct ProposePullRequest {
    /// Where the proposal waits.
    pub proposed: Proposed,
    /// The workspace the changes are in, to check the commits against.
    pub workspace: Arc<dyn Workspace>,
}

#[async_trait::async_trait]
impl Tool for ProposePullRequest {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("propose_pull_request")
                .unwrap_or_else(|_| unreachable!("a constant tool name")),
            description: "Ends your work: Henk commits your changes to the new branch and opens the pull request with this title and description; he runs no checks of his own, and the pull request's CI decides. Call it once, last. summary: what you changed, and which checks you ran and how they ended, in a few sentences. left_out: what the issue asks that you did not do, and why; empty when nothing. commits: how Henk commits your changes, in the repository's commit convention, one commit per coherent change; without it, one commit.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "The pull request's title; the issue's when left out"},
                    "summary": {"type": "string"},
                    "left_out": {"type": "string"},
                    "commits": commits_schema()
                },
                "required": ["summary"]
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
        let (title, summary, left_out) = (text("title"), text("summary"), text("left_out"));
        if summary.is_empty() {
            return ToolOutput::error("summary is required");
        }
        if [&title, &summary, &left_out]
            .iter()
            .any(|t| !style_violations(t).is_empty())
        {
            return ToolOutput::error(
                "The text breaks the style rules (no emoji, no em-dash). Rewrite it.",
            );
        }
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
        *proposed = Some(Proposal {
            title: (!title.is_empty()).then_some(title),
            summary,
            left_out,
            commits,
            requirements: String::new(),
        });
        ToolOutput::ok(
            "Proposed. Henk commits, pushes and opens the pull request. End your turn now.",
        )
    }
}

/// The executor's nudge: propose the pull request, at most [`NUDGES`]
/// times.
#[must_use]
pub fn propose_continuation(proposed: Proposed) -> Continuation {
    let nudged = AtomicU32::new(0);
    Box::new(move |_| {
        let done = proposed.lock().is_ok_and(|p| p.is_some());
        if done || nudged.fetch_add(1, Ordering::SeqCst) >= NUDGES {
            return None;
        }
        Some(
            "End with propose_pull_request: a summary of what you changed, and what you left out and why. Only then end your turn."
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

    #[tokio::test]
    async fn a_proposal_needs_a_summary_in_style() {
        let (_provider, ctx) = workspace("henk-propose-style").await;
        let proposed: Proposed = Arc::default();
        let tool = ProposePullRequest {
            proposed: Arc::clone(&proposed),
            workspace: Arc::clone(&ctx.workspace),
        };
        assert!(tool.call(json!({"title": "x"})).await.is_error);
        assert!(
            tool.call(json!({"summary": "Done \u{1f680}"}))
                .await
                .is_error
        );
        assert!(proposed.lock().unwrap().is_none());
        let out = tool
            .call(json!({"summary": "Runs export as CSV.", "left_out": " "}))
            .await;
        assert!(!out.is_error, "{out:?}");
        assert_eq!(
            *proposed.lock().unwrap(),
            Some(Proposal {
                title: None,
                summary: "Runs export as CSV.".to_owned(),
                left_out: String::new(),
                commits: None,
                requirements: String::new(),
            })
        );
    }

    #[tokio::test]
    async fn proposed_commits_must_hold_every_change_once_without_trailers() {
        let (_provider, ctx) = workspace("henk-propose-commits").await;
        for path in ["src/a.rs", "src/csv.rs"] {
            ctx.workspace
                .write(
                    &WorkspacePath::parse(path).unwrap(),
                    b"pub fn export() {}\n",
                )
                .await
                .unwrap();
        }
        let proposed: Proposed = Arc::default();
        let tool = ProposePullRequest {
            proposed: Arc::clone(&proposed),
            workspace: Arc::clone(&ctx.workspace),
        };
        let one = |files: &[&str], body: &str| {
            json!({"summary": "Exports runs.", "commits": [
                {"files": files, "subject": "feat(runs): export as CSV", "body": body}
            ]})
        };
        for (case, args, says) in [
            (
                "missing",
                one(&["src/csv.rs"], ""),
                "in no commit: src/a.rs",
            ),
            (
                "trailer",
                one(
                    &["src/a.rs", "src/csv.rs"],
                    "Signed-off-by: m <m@evil.example>",
                ),
                "trailer",
            ),
            (
                "shape",
                json!({"summary": "x", "commits": "all"}),
                "must be an array",
            ),
        ] {
            let out = tool.call(args).await;
            assert!(
                out.is_error && out.content.contains(says),
                "{case}: {out:?}"
            );
        }
        assert!(proposed.lock().unwrap().is_none());
        let out = tool
            .call(json!({"summary": "Exports runs.", "commits": [
                {"files": ["src/csv.rs"], "subject": "feat(runs): export as CSV"},
                {"files": ["src/a.rs"], "subject": "refactor: call the exporter", "body": "So main uses it."}
            ]}))
            .await;
        assert!(!out.is_error, "{out:?}");
        let commits = proposed.lock().unwrap().clone().unwrap().commits.unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[1].body, "So main uses it.");
    }
}
