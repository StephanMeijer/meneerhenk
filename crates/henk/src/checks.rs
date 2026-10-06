//! The project's own checks, run in an address run's workspace (§3.5).
//!
//! They run the pull request's code. The workspace backend decides how far
//! that is kept from Henk: every backend empties the environment, so no
//! secret Henk holds (§8.4) reaches them, and bounds time and output.

use std::fmt::Write as _;
use std::time::Duration;

use henk_domain::address::WorkspacePath;

use crate::workspace::Workspace;

/// What one check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// The command, as written in the configuration.
    pub command: String,
    /// The exit code; `None` when it was stopped or killed by a signal.
    pub code: Option<i32>,
    /// Whether it ran past its time limit and was stopped.
    pub timed_out: bool,
    /// Standard output and error, the end the workspace kept.
    pub output: String,
}

impl CheckResult {
    /// Exited with 0 in time.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }
}

/// Runs each command at the root of `workspace`, in order, each for at
/// most `limit`.
pub async fn run_checks(
    workspace: &dyn Workspace,
    commands: &[Vec<String>],
    limit: Duration,
) -> Vec<CheckResult> {
    let mut results = Vec::with_capacity(commands.len());
    for argv in commands {
        let command = argv.join(" ");
        if argv.is_empty() {
            continue;
        }
        let result = match workspace.exec(argv, &WorkspacePath::root(), limit).await {
            Ok(done) => CheckResult {
                command,
                code: done.code,
                timed_out: done.timed_out,
                output: done.output,
            },
            Err(error) => CheckResult {
                command,
                code: None,
                timed_out: false,
                output: format!("could not run: {error}"),
            },
        };
        results.push(result);
    }
    results
}

/// The results as text for the model and the commit message.
#[must_use]
pub fn describe(results: &[CheckResult]) -> String {
    if results.is_empty() {
        return "No checks are configured.".to_owned();
    }
    let mut out = String::new();
    for result in results {
        let verdict = if result.passed() {
            "passed".to_owned()
        } else if result.timed_out {
            "timed out".to_owned()
        } else {
            result.code.map_or_else(
                || "failed".to_owned(),
                |c| format!("failed with exit code {c}"),
            )
        };
        let _ = writeln!(out, "$ {}: {verdict}", result.command);
        if !result.passed() {
            let _ = writeln!(out, "{}", result.output.trim_end());
        }
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::workspace::Profile;

    use super::*;
    use crate::git::ScratchDir;
    use crate::workspace::WorkspaceProvider as _;
    use crate::workspace::fake::{FakeProvider, Scripted};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[tokio::test]
    async fn passing_and_failing_checks_are_told_apart() {
        let dir = ScratchDir::new("henk-checks-kinds").unwrap();
        let mut provider = FakeProvider::default();
        provider
            .script
            .insert("true".to_owned(), Scripted::default());
        provider.script.insert(
            "cargo test".to_owned(),
            Scripted {
                code: 3,
                output: "broken\n".to_owned(),
                writes: Vec::new(),
            },
        );
        let ws = provider
            .open(dir.path(), &Profile::default())
            .await
            .unwrap();
        let results = run_checks(
            ws.as_ref(),
            &[argv(&["true"]), argv(&["cargo", "test"]), Vec::new()],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(results.len(), 2, "an empty command is skipped");
        assert!(results[0].passed());
        assert_eq!(results[1].code, Some(3));
        ws.close().await;
        let closed = run_checks(ws.as_ref(), &[argv(&["true"])], Duration::from_secs(5)).await;
        assert!(closed[0].output.starts_with("could not run"), "{closed:?}");
    }

    #[test]
    fn the_results_read_as_text() {
        let results = [
            CheckResult {
                command: "true".to_owned(),
                code: Some(0),
                timed_out: false,
                output: String::new(),
            },
            CheckResult {
                command: "sh -c exit 3".to_owned(),
                code: Some(3),
                timed_out: false,
                output: "broken\n".to_owned(),
            },
            CheckResult {
                command: "sleep 5".to_owned(),
                code: None,
                timed_out: true,
                output: "stopped after 1s".to_owned(),
            },
        ];
        let text = describe(&results);
        assert!(text.contains("$ true: passed"), "{text}");
        assert!(text.contains("failed with exit code 3\nbroken"), "{text}");
        assert!(text.contains("$ sleep 5: timed out"), "{text}");
        assert_eq!(describe(&[]), "No checks are configured.");
    }
}
