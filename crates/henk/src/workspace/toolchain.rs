//! The repository's toolchain in a workspace (#93). With mise, every
//! command runs as `mise exec -- …`, so a check finds the tools and the
//! versions the repository's own `mise.toml` or `.tool-versions` names.
//! mise installs into the run's `HOME`, outside the tree, so nothing it
//! fetches becomes part of the changeset. Works on every backend.

use std::sync::Arc;
use std::time::Duration;

use henk_domain::address::WorkspacePath;

use super::{ExecResult, Exported, Hit, Workspace, WorkspaceError};

/// A workspace whose commands run through mise.
pub struct Mise {
    inner: Arc<dyn Workspace>,
}

impl Mise {
    /// `inner`, with every command run through `mise exec`.
    #[must_use]
    pub fn wrap(inner: Arc<dyn Workspace>) -> Arc<dyn Workspace> {
        Arc::new(Self { inner })
    }

    /// The argv that runs `argv` with the repository's tools.
    #[must_use]
    pub fn command(argv: &[String]) -> Vec<String> {
        ["mise", "exec", "--"]
            .iter()
            .map(|word| (*word).to_owned())
            .chain(argv.iter().cloned())
            .collect()
    }
}

#[async_trait::async_trait]
impl Workspace for Mise {
    async fn exec(
        &self,
        argv: &[String],
        cwd: &WorkspacePath,
        timeout: Duration,
    ) -> Result<ExecResult, WorkspaceError> {
        if argv.is_empty() {
            return Err(WorkspaceError::Refused("an empty command".to_owned()));
        }
        self.inner.exec(&Self::command(argv), cwd, timeout).await
    }

    async fn read(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, WorkspaceError> {
        self.inner.read(path, max_bytes).await
    }

    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        self.inner.write(path, content).await
    }

    async fn list(&self, dir: &WorkspacePath, cap: usize) -> Result<Vec<String>, WorkspaceError> {
        self.inner.list(dir, cap).await
    }

    async fn search(
        &self,
        dir: &WorkspacePath,
        needle: &str,
        max_file_bytes: u64,
        cap: usize,
    ) -> Result<Vec<Hit>, WorkspaceError> {
        self.inner.search(dir, needle, max_file_bytes, cap).await
    }

    async fn export(&self) -> Result<Vec<Exported>, WorkspaceError> {
        self.inner.export().await
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_command_runs_through_mise_exec() {
        let argv = ["cargo".to_owned(), "test".to_owned()];
        assert_eq!(
            Mise::command(&argv),
            ["mise", "exec", "--", "cargo", "test"]
        );
    }
}
