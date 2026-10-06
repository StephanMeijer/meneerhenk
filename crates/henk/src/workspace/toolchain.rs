//! The repository's toolchain in a workspace (#93). With mise, every
//! command runs as `mise exec -- …`, so a check finds the tools and the
//! versions the repository's own `mise.toml` or `.tool-versions` names.
//! mise installs into the run's `HOME`, outside the tree. Works on every
//! backend.
//!
//! Every mise command runs in mise's safe mode (`MISE_SAFE=1`), the mode
//! mise has for configuration nobody trusted: it reads the tool versions
//! and refuses to run anything the repository's file defines, such as
//! `exec()` templates, `_.source`, hooks, tasks and plugin scripts, and
//! ignores its `[env]`. So the file stays data (§8.3), and Henk never runs
//! `mise trust`. A mise without safe mode is refused before anything else.

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
        safe(&["exec", "--"]).chain(argv.iter().cloned()).collect()
    }

    /// The argv that prints `true` when this mise runs in safe mode. A mise
    /// too old to have it fails on the unknown setting.
    #[must_use]
    pub fn safe_mode() -> Vec<String> {
        safe(&["settings", "get", "safe"]).collect()
    }

    /// The argv that installs the tools the repository names.
    #[must_use]
    pub fn install() -> Vec<String> {
        safe(&["install"]).collect()
    }
}

/// `mise` with `args`, in safe mode. Commands run with an empty
/// environment, so the setting goes in through `env`.
fn safe<'a>(args: &'a [&'a str]) -> impl Iterator<Item = String> + 'a {
    ["env", "MISE_SAFE=1", "mise"]
        .iter()
        .chain(args)
        .map(|word| (*word).to_owned())
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

    async fn baseline(&self) -> Result<(), WorkspaceError> {
        self.inner.baseline().await
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
            ["env", "MISE_SAFE=1", "mise", "exec", "--", "cargo", "test"]
        );
    }

    #[test]
    fn every_mise_command_runs_in_safe_mode_and_none_trusts() {
        for argv in [Mise::command(&[]), Mise::safe_mode(), Mise::install()] {
            assert_eq!(argv.get(..3).unwrap(), ["env", "MISE_SAFE=1", "mise"]);
            assert!(!argv.iter().any(|word| word == "trust"), "{argv:?}");
        }
    }
}
