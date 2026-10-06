//! Workspaces (§3.5): the one place an address run's model reads, edits and
//! runs the project's checks. The tools talk only to a [`Workspace`]; which
//! backend serves it, the host today and a container or microVM later, is a
//! configuration choice (`[workspace]`).
//!
//! Nothing leaves a workspace but its changeset, exported by Henk's code and
//! checked against `henk_domain::workspace::changeset_refusal` before it is
//! applied to a fresh checkout that nothing ever ran in.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use henk_domain::address::{PathError, WorkspacePath};
use henk_domain::workspace::{Change, ChangeKind, FileMode, Profile, RawChange, changeset_refusal};

#[cfg(test)]
pub mod fake;
pub mod host;
pub mod traced;

/// Why a workspace operation failed. The text goes back to the model, so it
/// names the path and never a host path.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The path is refused before anything is touched.
    #[error(transparent)]
    Path(#[from] PathError),
    /// Nothing is there.
    #[error("{0} does not exist")]
    NotFound(String),
    /// The path exists but may not be used this way.
    #[error("{0}")]
    Refused(String),
    /// The file system said no.
    #[error("{path}: {source}")]
    Io {
        /// The workspace path.
        path: String,
        /// What went wrong.
        source: std::io::Error,
    },
    /// The backend itself failed or is closed.
    #[error("the workspace failed: {0}")]
    Backend(String),
}

/// What one command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// The exit code; `None` when it was stopped or killed by a signal.
    pub code: Option<i32>,
    /// Whether it ran past its time limit and was stopped.
    pub timed_out: bool,
    /// Standard output then standard error, the end kept within the
    /// profile's `output_bytes`.
    pub output: String,
    /// How long it ran.
    pub duration: Duration,
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The file, relative to the workspace root.
    pub path: String,
    /// The line number, from 1.
    pub line: usize,
    /// The line, trimmed.
    pub text: String,
}

/// One change as a backend exported it: what git calls it, and the new
/// content of a file that is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    /// Path, mode and whether it is gone, for the policy.
    pub raw: RawChange,
    /// The new content; empty for a deletion, a link or a submodule.
    pub content: Vec<u8>,
}

/// Checks an exported changeset against the policy (§3.5) and turns it into
/// changes to apply. It is refused whole or taken whole.
///
/// # Errors
///
/// Returns the refusal, in words, when any change may not be pushed.
pub fn checked_changeset(exported: Vec<Exported>, max_files: usize) -> Result<Vec<Change>, String> {
    let raw: Vec<RawChange> = exported.iter().map(|e| e.raw.clone()).collect();
    if let Some(why) = changeset_refusal(&raw, max_files) {
        return Err(why);
    }
    exported
        .into_iter()
        .map(|e| {
            let path = WorkspacePath::parse(&e.raw.path).map_err(|error| error.to_string())?;
            let kind = if e.raw.deleted {
                ChangeKind::Delete
            } else {
                ChangeKind::Write {
                    content: e.content,
                    executable: e.raw.mode == FileMode::Executable,
                }
            };
            Ok(Change { path, kind })
        })
        .collect()
}

/// A workspace: a copy of one pull request's files where a model's tools
/// read, write and run commands. Paths are relative to its root; `.git` is
/// never part of it.
///
/// Every backend destroys the workspace on [`Workspace::close`] and also
/// when it is dropped, so a run that fails, is cancelled or is aborted
/// leaves nothing behind.
#[async_trait::async_trait]
pub trait Workspace: Send + Sync {
    /// Runs `argv` in `cwd` with an empty environment, for at most
    /// `timeout` and never longer than the profile allows.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when `cwd` is refused or the backend
    /// failed; a command that fails is an [`ExecResult`], not an error.
    async fn exec(
        &self,
        argv: &[String],
        cwd: &WorkspacePath,
        timeout: Duration,
    ) -> Result<ExecResult, WorkspaceError>;

    /// The bytes of a file, refused when it is larger than `max_bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the path is refused, missing, too
    /// large or unreadable.
    async fn read(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, WorkspaceError>;

    /// Writes a whole file, creating it and its directories when needed. An
    /// existing file keeps its mode; a new one is not executable.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the path is refused or the write fails.
    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError>;

    /// The files under `dir`, sorted, at most `cap`. `.git` is left out and
    /// symbolic links are not followed.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when `dir` is refused or missing.
    async fn list(&self, dir: &WorkspacePath, cap: usize) -> Result<Vec<String>, WorkspaceError>;

    /// Lines containing `needle` in the files under `dir`, at most `cap`.
    /// Files larger than `max_file_bytes` or not text are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when `dir` is refused or missing.
    async fn search(
        &self,
        dir: &WorkspacePath,
        needle: &str,
        max_file_bytes: u64,
        cap: usize,
    ) -> Result<Vec<Hit>, WorkspaceError>;

    /// Every change since the workspace was opened, read from the backend's
    /// own record of it and never by following a path in the tree.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the backend cannot say.
    async fn export(&self) -> Result<Vec<Exported>, WorkspaceError>;

    /// Destroys the workspace. Calling it again does nothing.
    async fn close(&self);
}

/// Opens workspaces of one backend.
#[async_trait::async_trait]
pub trait WorkspaceProvider: Send + Sync {
    /// Imports the files of the checkout at `source`, without its `.git`,
    /// into a new workspace run as `profile` says. The caller may remove
    /// `source` once this returns.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the workspace cannot be made.
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError>;
}

/// The last `cap` bytes of `text`, on a character boundary, marked as cut.
#[must_use]
pub fn tail(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut start = text.len().saturating_sub(cap);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[... cut ...]\n{}", text.get(start..).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn exported(path: &str, mode: FileMode, deleted: bool) -> Exported {
        Exported {
            raw: RawChange {
                path: path.to_owned(),
                mode,
                deleted,
            },
            content: if deleted { Vec::new() } else { b"x\n".to_vec() },
        }
    }

    #[test]
    fn long_output_keeps_its_end() {
        let cap = 20 * 1024;
        let text = format!("{}END", "x".repeat(cap * 2));
        let kept = tail(&text, cap);
        assert!(kept.ends_with("END"));
        assert!(kept.len() <= cap + 20);
        assert_eq!(tail("short", cap), "short");
        assert_eq!(tail("\u{e9}\u{e9}", 3), "[... cut ...]\n\u{e9}");
    }

    #[test]
    fn a_changeset_is_taken_whole_or_refused_whole() {
        let changes = checked_changeset(
            vec![
                exported("src/a.rs", FileMode::Regular, false),
                exported("run.sh", FileMode::Executable, false),
                exported("old.md", FileMode::Regular, true),
            ],
            3,
        )
        .unwrap();
        assert_eq!(
            changes,
            [
                Change {
                    path: WorkspacePath::parse("src/a.rs").unwrap(),
                    kind: ChangeKind::Write {
                        content: b"x\n".to_vec(),
                        executable: false
                    }
                },
                Change {
                    path: WorkspacePath::parse("run.sh").unwrap(),
                    kind: ChangeKind::Write {
                        content: b"x\n".to_vec(),
                        executable: true
                    }
                },
                Change {
                    path: WorkspacePath::parse("old.md").unwrap(),
                    kind: ChangeKind::Delete
                },
            ]
        );
        assert!(
            checked_changeset(
                vec![
                    exported("src/a.rs", FileMode::Regular, false),
                    exported("link", FileMode::Symlink, false),
                ],
                5
            )
            .is_err()
        );
    }
}
