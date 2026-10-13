//! The tool an executor ends an execute run with (#329): it proposes the
//! pull request, and Henk's code checks, commits, pushes and opens it.
//! Nothing here reaches the platform.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{Continuation, Tool, ToolOutput};
use henk_domain::text::style_violations;
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

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
}

/// Where the proposal waits for Henk's code.
pub type Proposed = Arc<Mutex<Option<Proposal>>>;

/// `propose_pull_request`: the executor is done.
pub struct ProposePullRequest(pub Proposed);

#[async_trait::async_trait]
impl Tool for ProposePullRequest {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("propose_pull_request")
                .unwrap_or_else(|_| unreachable!("a constant tool name")),
            description: "Ends your work: Henk commits your changes to the new branch and opens the pull request with this title and description; he runs no checks of his own, and the pull request's CI decides. Call it once, last. summary: what you changed, and which checks you ran and how they ended, in a few sentences. left_out: what the issue asks that you did not do, and why; empty when nothing.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "The pull request's title; the issue's when left out"},
                    "summary": {"type": "string"},
                    "left_out": {"type": "string"}
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
        let Ok(mut proposed) = self.0.lock() else {
            return ToolOutput::error("proposal unavailable");
        };
        *proposed = Some(Proposal {
            title: (!title.is_empty()).then_some(title),
            summary,
            left_out,
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
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn a_proposal_needs_a_summary_in_style() {
        let proposed: Proposed = Arc::default();
        let tool = ProposePullRequest(Arc::clone(&proposed));
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
            })
        );
    }
}
