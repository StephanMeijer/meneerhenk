//! A workspace that records every command it runs on the run's timeline,
//! so `henk runs show` says what ran, how it ended and what it printed
//! (§8.6).

use std::sync::Arc;
use std::time::Duration;

use henk_domain::address::WorkspacePath;
use henk_domain::run::RunId;
use henk_store::RunStore;
use tracing::warn;

use super::{ExecResult, Exported, Hit, Workspace, WorkspaceError};

/// Wraps a workspace; every `exec` is passed through and recorded.
pub struct Traced {
    inner: Arc<dyn Workspace>,
    store: Arc<dyn RunStore>,
    run: RunId,
}

impl Traced {
    /// Records `inner`'s commands on `run`.
    #[must_use]
    pub fn new(inner: Arc<dyn Workspace>, store: Arc<dyn RunStore>, run: RunId) -> Self {
        Self { inner, store, run }
    }
}

/// One timeline line: `exec <argv> exit <code> in <ms> ms`, then the kept
/// output.
fn line(argv: &[String], result: &Result<ExecResult, WorkspaceError>) -> String {
    let command = argv.join(" ");
    match result {
        Ok(done) => {
            let end = if done.timed_out {
                "timeout".to_owned()
            } else {
                done.code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string())
            };
            format!(
                "exec {command} exit {end} in {} ms\n{}",
                done.duration.as_millis(),
                done.output.trim_end()
            )
        }
        Err(error) => format!("exec {command} refused: {error}"),
    }
}

#[async_trait::async_trait]
impl Workspace for Traced {
    async fn exec(
        &self,
        argv: &[String],
        cwd: &WorkspacePath,
        timeout: Duration,
    ) -> Result<ExecResult, WorkspaceError> {
        let result = self.inner.exec(argv, cwd, timeout).await;
        if let Err(error) = self
            .store
            .event(&self.run, "info", &line(argv, &result))
            .await
        {
            warn!(%error, "could not record a command");
        }
        result
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

    #[tokio::test]
    async fn every_command_is_on_the_runs_timeline() {
        let store: Arc<dyn RunStore> = Arc::new(henk_store::SqliteStore::in_memory().unwrap());
        let run = RunId::parse("r-trace-1").unwrap();
        store
            .create_run(&henk_store::NewRun {
                id: run.clone(),
                kind: henk_domain::run::RunKind::Address,
                platform: henk_domain::allowlist::Platform::GitHub,
                repo: "o/r".to_owned(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "test".to_owned(),
                link: String::new(),
            })
            .await
            .unwrap();
        let mut provider = FakeProvider::default();
        provider.script.insert(
            "cargo test".to_owned(),
            Scripted {
                code: 101,
                output: "test failed\n".to_owned(),
                writes: Vec::new(),
                delay: Duration::ZERO,
            },
        );
        let src = ScratchDir::new("henk-trace-src").unwrap();
        let inner = provider
            .open(src.path(), &Profile::default())
            .await
            .unwrap();
        let ws = Traced::new(inner, Arc::clone(&store), run.clone());
        let argv = vec!["cargo".to_owned(), "test".to_owned()];
        let result = ws
            .exec(&argv, &WorkspacePath::root(), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(result.code, Some(101));
        let events = store.events(&run).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, "info");
        assert_eq!(
            events[0].message,
            "exec cargo test exit 101 in 1 ms\ntest failed"
        );
        ws.close().await;
        assert!(provider.closed());
    }
}
