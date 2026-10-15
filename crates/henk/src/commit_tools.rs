//! The commits an executor proposes for its changes (#400): which files
//! each holds, and its subject and body in the repository's own commit
//! convention. Henk checks them, adds the trailers, commits and pushes;
//! nothing here reaches git or the platform.

use std::sync::{Arc, Mutex};

use henk_agent::{Tool, ToolOutput};
use henk_domain::commit::{CommitProposal, MAX_COMMITS, MAX_SUBJECT, planned_commits};
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

use crate::workspace::Workspace;

/// Where the executor's commits wait for Henk's code.
pub type ProposedCommits = Arc<Mutex<Option<Vec<CommitProposal>>>>;

/// The schema of a `commits` argument.
#[must_use]
pub fn commits_schema() -> Value {
    json!({
        "type": "array",
        "description": format!("At most {MAX_COMMITS} commits, applied in order. Every file you changed is in exactly one."),
        "items": {
            "type": "object",
            "properties": {
                "files": {"type": "array", "items": {"type": "string"}, "description": "The changed files this commit holds, as the workspace names them"},
                "subject": {"type": "string", "description": format!("One line of at most {MAX_SUBJECT} characters, in the repository's commit convention")},
                "body": {"type": "string", "description": "Why, in plain text; names the finding it fixes. No trailers: Henk adds them"}
            },
            "required": ["files", "subject"]
        }
    })
}

/// The commits in a `commits` argument.
///
/// # Errors
///
/// Returns why when it is not an array of commits.
pub fn parse_commits(value: &Value) -> Result<Vec<CommitProposal>, String> {
    let Some(items) = value.as_array() else {
        return Err("commits must be an array".to_owned());
    };
    items
        .iter()
        .enumerate()
        .map(|(n, item)| {
            let files = item
                .get("files")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("commit {} has no files array", n + 1))?
                .iter()
                .map(|f| {
                    f.as_str()
                        .map(|f| f.trim().to_owned())
                        .ok_or_else(|| format!("commit {} has a file that is not a string", n + 1))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let text = |key: &str| {
                item.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            Ok(CommitProposal {
                files,
                subject: text("subject"),
                body: text("body"),
            })
        })
        .collect()
}

/// Checks `commits` against what changed in `workspace` now, so the
/// executor hears what is wrong while it can still fix it. Henk checks
/// them again against the changeset he pushes.
///
/// # Errors
///
/// Returns why they cannot be made, in words the executor can act on.
pub async fn check_commits(
    workspace: &dyn Workspace,
    commits: &[CommitProposal],
) -> Result<(), String> {
    let exported = workspace
        .export()
        .await
        .map_err(|error| format!("could not list your changes: {error}"))?;
    let changed: Vec<&str> = exported.iter().map(|e| e.raw.path.as_str()).collect();
    planned_commits(commits, &changed).map(|_| ())
}

/// `propose_commits`: how the review loop's executor wants this round's
/// changes committed.
pub struct ProposeCommits {
    /// Where the proposal waits.
    pub proposed: ProposedCommits,
    /// The workspace the changes are in.
    pub workspace: Arc<dyn Workspace>,
}

#[async_trait::async_trait]
impl Tool for ProposeCommits {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("propose_commits")
                .unwrap_or_else(|_| unreachable!("a constant tool name")),
            description: "Says how Henk commits this round's changes once your verdicts are given: one or more commits, each with the files it holds, a subject and a body in the repository's commit convention, one commit per coherent change. Every file you changed is in exactly one commit. Call it last in the round; a later call replaces the earlier one. Without it, Henk pushes the round as one commit.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"commits": commits_schema()},
                "required": ["commits"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let commits = match parse_commits(args.get("commits").unwrap_or(&Value::Null)) {
            Ok(commits) => commits,
            Err(why) => return ToolOutput::error(format!("Not proposed: {why}.")),
        };
        if let Err(why) = check_commits(&*self.workspace, &commits).await {
            return ToolOutput::error(format!("Not proposed: {why}."));
        }
        let count = commits.len();
        let Ok(mut proposed) = self.proposed.lock() else {
            return ToolOutput::error("proposal unavailable");
        };
        *proposed = Some(commits);
        ToolOutput::ok(format!(
            "Proposed {count} commit(s). Henk commits and pushes them after your round."
        ))
    }
}

/// How many of the branch's latest commit subjects the executor is shown.
pub const RECENT_COMMITS: usize = 20;

/// The branch's latest commit subjects as the executor's prompt lists them:
/// data that shows the repository's commit convention, never instructions.
#[must_use]
pub fn recent_commits_text(subjects: &[String]) -> String {
    if subjects.is_empty() {
        return "(none)".to_owned();
    }
    subjects
        .iter()
        .map(|s| format!("- {}", s.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect::<Vec<_>>()
        .join("\n")
}
