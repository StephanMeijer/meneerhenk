//! The project's own checks, run in an address run's checkout (§3.5).
//!
//! They run the pull request's code. Until Henk runs them in a container,
//! the guard is the environment: it is emptied, so no secret Henk holds
//! (§8.4) reaches them, and each command has a time limit.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Output kept per command for the model: the end, where errors are.
pub const OUTPUT_CAP: usize = 20 * 1024;

/// What one check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// The command, as written in the configuration.
    pub command: String,
    /// The exit code; `None` when it was stopped or killed by a signal.
    pub code: Option<i32>,
    /// Whether it ran past its time limit and was stopped.
    pub timed_out: bool,
    /// Standard output and error, the last [`OUTPUT_CAP`] bytes.
    pub output: String,
}

impl CheckResult {
    /// Exited with 0 in time.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }
}

/// The last `cap` bytes of `text`, on a character boundary.
fn tail(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[... cut ...]\n{}", text.get(start..).unwrap_or_default())
}

/// Runs each command in `dir`, in order, with an empty environment.
pub async fn run_checks(dir: &Path, commands: &[Vec<String>], limit: Duration) -> Vec<CheckResult> {
    let mut results = Vec::with_capacity(commands.len());
    for argv in commands {
        let shown = argv.join(" ");
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        let child = Command::new(program)
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", dir)
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                results.push(CheckResult {
                    command: shown,
                    code: None,
                    timed_out: false,
                    output: format!("could not start: {error}"),
                });
                continue;
            }
        };
        match tokio::time::timeout(limit, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                let _ = write!(text, "{}", String::from_utf8_lossy(&output.stderr));
                results.push(CheckResult {
                    command: shown,
                    code: output.status.code(),
                    timed_out: false,
                    output: tail(&text, OUTPUT_CAP),
                });
            }
            Ok(Err(error)) => results.push(CheckResult {
                command: shown,
                code: None,
                timed_out: false,
                output: format!("could not wait for it: {error}"),
            }),
            // Dropping the future kills the child (`kill_on_drop`).
            Err(_) => results.push(CheckResult {
                command: shown,
                code: None,
                timed_out: true,
                output: format!("stopped after {}s", limit.as_secs()),
            }),
        }
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

    use super::*;
    use crate::git::ScratchDir;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[tokio::test]
    async fn passing_failing_and_slow_commands_are_told_apart() {
        let dir = ScratchDir::new("henk-checks-kinds").unwrap();
        let results = run_checks(
            dir.path(),
            &[
                argv(&["true"]),
                argv(&["sh", "-c", "echo broken >&2; exit 3"]),
                argv(&["sleep", "5"]),
            ],
            Duration::from_millis(300),
        )
        .await;
        assert!(results[0].passed());
        assert_eq!(results[1].code, Some(3));
        assert_eq!(results[1].output.trim(), "broken");
        assert!(results[2].timed_out);
        let text = describe(&results);
        assert!(text.contains("$ true: passed"), "{text}");
        assert!(text.contains("failed with exit code 3\nbroken"), "{text}");
        assert!(text.contains("$ sleep 5: timed out"), "{text}");
    }

    #[tokio::test]
    async fn a_check_sees_none_of_henks_environment() {
        let dir = ScratchDir::new("henk-checks-env").unwrap();
        let results = run_checks(dir.path(), &[argv(&["env"])], Duration::from_secs(5)).await;
        let names: Vec<&str> = results[0]
            .output
            .lines()
            .filter_map(|line| line.split('=').next())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, ["HOME", "LANG", "PATH"], "{}", results[0].output);
        assert!(
            results[0]
                .output
                .contains(&format!("HOME={}", dir.path().display()))
        );
    }

    #[test]
    fn long_output_keeps_its_end() {
        let text = format!("{}END", "x".repeat(OUTPUT_CAP * 2));
        let kept = tail(&text, OUTPUT_CAP);
        assert!(kept.ends_with("END"));
        assert!(kept.len() <= OUTPUT_CAP + 20);
        assert_eq!(tail("short", OUTPUT_CAP), "short");
    }
}
