//! A workspace that records every command it runs on the run's timeline,
//! so `henk runs show` says what ran, how it ended and what it printed
//! (§8.6).

use std::sync::Arc;
use std::time::Duration;

use henk_domain::path::WorkspacePath;
use henk_domain::run::RunId;
use henk_store::RunStore;
use tracing::warn;

use henk_domain::ignore::PathFilter;

use super::{ExecResult, Exported, Hit, Pattern, Workspace, WorkspaceError};

/// Wraps a workspace; every `exec` is passed through and recorded.
pub struct Traced {
    inner: Arc<dyn Workspace>,
    store: Arc<dyn RunStore>,
    run: RunId,
    /// Which of the run's workspaces this is, such as a review lane's name,
    /// when the run has more than one.
    label: Option<String>,
}

impl Traced {
    /// Records `inner`'s commands on `run`.
    #[must_use]
    pub fn new(inner: Arc<dyn Workspace>, store: Arc<dyn RunStore>, run: RunId) -> Self {
        Self {
            inner,
            store,
            run,
            label: None,
        }
    }

    /// Starts every line with `label`, so a run with several workspaces,
    /// a review's lanes (#85), says which ran what.
    #[must_use]
    pub fn labelled(mut self, label: String) -> Self {
        self.label = Some(label);
        self
    }

    /// `result`, with a failure of the workspace itself logged once and put
    /// on the run's timeline, saying which run, session, workspace and
    /// operation (#339). A model's own mistake, such as a path that is not
    /// there, is an ordinary answer and goes by.
    async fn noted<T>(
        &self,
        operation: &str,
        result: Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        if let Err(error) = &result
            && matches!(
                error,
                WorkspaceError::Backend(_) | WorkspaceError::Unreachable(_)
            )
        {
            let at = self.inner.location();
            warn!(
                run = %self.run,
                session = self.label.as_deref().unwrap_or(""),
                workspace = at.as_deref().unwrap_or(""),
                operation,
                %error,
                "workspace operation failed"
            );
            let place = at.map_or_else(String::new, |at| format!(" on {at}"));
            let text = format!("workspace {operation} failed{place}: {error}");
            let text = match &self.label {
                Some(label) => format!("{label}: {text}"),
                None => text,
            };
            if let Err(store) = self.store.event(&self.run, "warn", &text).await {
                warn!(error = %store, "could not record a workspace failure");
            }
        }
        result
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
        let text = match &self.label {
            Some(label) => format!("{label}: {}", line(argv, &result)),
            None => line(argv, &result),
        };
        if let Err(error) = self.store.event(&self.run, "info", &text).await {
            warn!(%error, "could not record a command");
        }
        if let Err(error) = &result
            && matches!(
                error,
                WorkspaceError::Backend(_) | WorkspaceError::Unreachable(_)
            )
        {
            // On the timeline already, as the exec line; logged here.
            let at = self.inner.location();
            warn!(
                run = %self.run,
                session = self.label.as_deref().unwrap_or(""),
                workspace = at.as_deref().unwrap_or(""),
                operation = "exec",
                %error,
                "workspace operation failed"
            );
        }
        result
    }

    async fn read(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, WorkspaceError> {
        let result = self.inner.read(path, max_bytes).await;
        self.noted("read", result).await
    }

    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        let result = self.inner.write(path, content).await;
        self.noted("write", result).await
    }

    async fn list(
        &self,
        dir: &WorkspacePath,
        only: Option<&PathFilter>,
        cap: usize,
    ) -> Result<Vec<String>, WorkspaceError> {
        let result = self.inner.list(dir, only, cap).await;
        self.noted("list", result).await
    }

    async fn search(
        &self,
        dir: &WorkspacePath,
        pattern: &Pattern,
        only: Option<&PathFilter>,
        context: usize,
        max_file_bytes: u64,
        cap: usize,
    ) -> Result<Vec<Hit>, WorkspaceError> {
        let result = self
            .inner
            .search(dir, pattern, only, context, max_file_bytes, cap)
            .await;
        self.noted("search", result).await
    }

    async fn export(&self) -> Result<Vec<Exported>, WorkspaceError> {
        let result = self.inner.export().await;
        self.noted("export", result).await
    }

    async fn baseline(&self) -> Result<(), WorkspaceError> {
        let result = self.inner.baseline().await;
        self.noted("baseline", result).await
    }

    fn location(&self) -> Option<String> {
        self.inner.location()
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

        let inner = provider
            .open(src.path(), &Profile::default())
            .await
            .unwrap();
        let lane =
            Traced::new(inner, Arc::clone(&store), run.clone()).labelled("lane-a".to_owned());
        lane.exec(&argv, &WorkspacePath::root(), Duration::from_secs(1))
            .await
            .unwrap();
        let events = store.events(&run).await.unwrap();
        assert_eq!(
            events[1].message, "lane-a: exec cargo test exit 101 in 1 ms\ntest failed",
            "a review's lanes say which ran what"
        );
    }

    /// A workspace whose reads fail in the backend and whose listings find
    /// nothing, on a named host.
    struct Failing;

    #[async_trait::async_trait]
    impl Workspace for Failing {
        async fn exec(
            &self,
            _: &[String],
            _: &WorkspacePath,
            _: Duration,
        ) -> Result<ExecResult, WorkspaceError> {
            Err(WorkspaceError::NotFound("bash".to_owned()))
        }
        async fn read(&self, _: &WorkspacePath, _: u64) -> Result<Vec<u8>, WorkspaceError> {
            Err(WorkspaceError::Backend(
                "reading src/a.rs failed: exit 1, nothing on stderr".to_owned(),
            ))
        }
        async fn write(&self, _: &WorkspacePath, _: &[u8]) -> Result<(), WorkspaceError> {
            Ok(())
        }
        async fn list(
            &self,
            dir: &WorkspacePath,
            _: Option<&PathFilter>,
            _: usize,
        ) -> Result<Vec<String>, WorkspaceError> {
            Err(WorkspaceError::NotFound(dir.to_string()))
        }
        async fn search(
            &self,
            _: &WorkspacePath,
            _: &Pattern,
            _: Option<&PathFilter>,
            _: usize,
            _: u64,
            _: usize,
        ) -> Result<Vec<Hit>, WorkspaceError> {
            Err(WorkspaceError::Unreachable(
                "the sandbox host 10.0.0.9:22 did not answer within 30s".to_owned(),
            ))
        }
        async fn export(&self) -> Result<Vec<Exported>, WorkspaceError> {
            Ok(Vec::new())
        }
        async fn baseline(&self) -> Result<(), WorkspaceError> {
            Ok(())
        }
        fn location(&self) -> Option<String> {
            Some("ssh goat (root@goat.example:22)".to_owned())
        }
        async fn close(&self) {}
    }

    #[tokio::test]
    async fn a_failure_of_the_workspace_is_on_the_timeline_with_where_and_what() {
        let store: Arc<dyn RunStore> = Arc::new(henk_store::SqliteStore::in_memory().unwrap());
        let run = RunId::parse("r-trace-fail").unwrap();
        store
            .create_run(&henk_store::NewRun {
                id: run.clone(),
                kind: henk_domain::run::RunKind::Review,
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
        let ws = Traced::new(Arc::new(Failing), Arc::clone(&store), run.clone())
            .labelled("reviewer".to_owned());
        let path = WorkspacePath::parse("src/a.rs").unwrap();
        assert!(ws.read(&path, 100).await.is_err());
        assert!(
            ws.list(&WorkspacePath::root(), None, 10).await.is_err(),
            "a listing of nothing is the model's answer"
        );
        let pattern = Pattern::parse("x").unwrap();
        assert!(
            ws.search(&WorkspacePath::root(), &pattern, None, 0, 100, 10)
                .await
                .is_err()
        );
        let events: Vec<(String, String)> = store
            .events(&run)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.level, e.message))
            .collect();
        assert_eq!(
            events,
            [
                (
                    "warn".to_owned(),
                    "reviewer: workspace read failed on ssh goat (root@goat.example:22): the workspace failed: reading src/a.rs failed: exit 1, nothing on stderr".to_owned()
                ),
                (
                    "warn".to_owned(),
                    "reviewer: workspace search failed on ssh goat (root@goat.example:22): the workspace failed: the sandbox host 10.0.0.9:22 did not answer within 30s".to_owned()
                ),
            ]
        );
    }
}
