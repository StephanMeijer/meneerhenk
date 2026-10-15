//! A workspace that records every command it runs on the run's timeline,
//! so `henk runs show` says what ran, how it ended and what it printed
//! (§8.6).

use std::sync::Arc;
use std::time::Duration;

use henk_domain::path::WorkspacePath;
use henk_domain::run::RunId;
use henk_store::RunStore;
use tracing::{Level, debug, info, warn};

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

/// A command that ran this long is logged at INFO though it succeeded.
const LONG: Duration = Duration::from_mins(1);
/// The most of a command's text a log line holds.
const COMMAND_CHARS: usize = 200;
/// The end of a command's output a log line holds: lines, then bytes.
const TAIL_LINES: usize = 20;
const TAIL_BYTES: usize = 2_000;

/// How loudly a command is logged (#345): a command stopped at its limit
/// warns, one that failed or ran a minute or more informs, the rest is
/// debug.
fn level(done: &ExecResult) -> Level {
    if done.timed_out {
        Level::WARN
    } else if done.code != Some(0) || done.duration >= LONG {
        Level::INFO
    } else {
        Level::DEBUG
    }
}

/// The last `lines` lines of `output`, at most `bytes` of them, cut on a
/// character boundary.
fn tail(output: &str, lines: usize, bytes: usize) -> &str {
    let output = output.trim_end();
    let start = output
        .rmatch_indices('\n')
        .nth(lines.saturating_sub(1))
        .map_or(0, |(at, _)| at + 1)
        .max(output.len().saturating_sub(bytes));
    let start = output.ceil_char_boundary(start);
    output.get(start..).unwrap_or_default()
}

/// The command as a log field: its words joined, control characters
/// escaped, at most [`COMMAND_CHARS`] characters.
fn command_text(argv: &[String]) -> String {
    let joined = crate::runs::terminal_safe(&argv.join(" ")).replace('\n', "\\n");
    if joined.chars().count() <= COMMAND_CHARS {
        return joined;
    }
    let mut cut: String = joined.chars().take(COMMAND_CHARS).collect();
    cut.push('…');
    cut
}

impl Traced {
    /// Logs one command (#345): what the run's timeline holds, in Henk's
    /// own log, since a workspace's commands never reach its Pod's log.
    fn log(&self, argv: &[String], done: &ExecResult) {
        let run = self.run.as_str();
        let session = self.label.as_deref().unwrap_or("");
        let at = self.inner.location();
        let workspace = at.as_deref().unwrap_or("");
        let command = command_text(argv);
        let exit = if done.timed_out {
            "timeout".to_owned()
        } else {
            done.code
                .map_or_else(|| "signal".to_owned(), |code| code.to_string())
        };
        let duration_ms = u64::try_from(done.duration.as_millis()).unwrap_or(u64::MAX);
        let output = tail(&done.output, TAIL_LINES, TAIL_BYTES);
        macro_rules! at_level {
            ($macro:ident) => {
                $macro!(
                    run,
                    session,
                    workspace,
                    command,
                    exit,
                    duration_ms,
                    timed_out = done.timed_out,
                    output,
                    "workspace command"
                )
            };
        }
        match level(done) {
            Level::WARN => at_level!(warn),
            Level::INFO => at_level!(info),
            _ => at_level!(debug),
        }
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
        match &result {
            Ok(done) => self.log(argv, done),
            // On the timeline already, as the exec line; logged here.
            Err(error)
                if matches!(
                    error,
                    WorkspaceError::Backend(_) | WorkspaceError::Unreachable(_)
                ) =>
            {
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
            // The model's own mistake, such as a directory that is not
            // there: an answer, not a failure.
            Err(error) => debug!(
                run = %self.run,
                session = self.label.as_deref().unwrap_or(""),
                command = command_text(argv),
                %error,
                "workspace command refused"
            ),
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

    async fn remove(&self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
        let result = self.inner.remove(path).await;
        self.noted("remove", result).await
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
        async fn remove(&self, _: &WorkspacePath) -> Result<(), WorkspaceError> {
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

    fn done(code: Option<i32>, secs: u64, timed_out: bool) -> ExecResult {
        ExecResult {
            code,
            timed_out,
            output: String::new(),
            duration: Duration::from_secs(secs),
        }
    }

    #[test]
    fn a_command_is_logged_as_loudly_as_it_matters() {
        assert_eq!(level(&done(Some(0), 2, false)), Level::DEBUG);
        assert_eq!(level(&done(Some(1), 2, false)), Level::INFO, "failed");
        assert_eq!(level(&done(None, 2, false)), Level::INFO, "a signal");
        assert_eq!(level(&done(Some(0), 75, false)), Level::INFO, "long");
        assert_eq!(level(&done(None, 600, true)), Level::WARN, "at its limit");
    }

    #[test]
    fn the_log_holds_the_end_of_the_output() {
        let output = (1..=30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let kept = tail(&output, 20, 2_000);
        assert!(
            kept.starts_with("line 11\n") && kept.ends_with("line 30"),
            "{kept}"
        );
        assert_eq!(tail("", 20, 2_000), "");
        assert_eq!(tail("one line", 20, 2_000), "one line");
        let wide = "é".repeat(2_000);
        let kept = tail(&wide, 20, 2_001);
        assert!(
            kept.len() <= 2_001 && kept.chars().all(|c| c == 'é'),
            "cut on a character"
        );
    }

    #[test]
    fn a_long_or_odd_command_is_cut_and_escaped() {
        let argv = vec!["bash".to_owned(), "-c".to_owned(), "x".repeat(300)];
        let text = command_text(&argv);
        assert_eq!(text.chars().count(), 201);
        assert!(
            text.starts_with("bash -c xxx") && text.ends_with('…'),
            "{text}"
        );
        let odd = command_text(&["printf".to_owned(), "a\u{1b}[2Jb\nc".to_owned()]);
        assert_eq!(odd, "printf a\\u{1b}[2Jb\\nc");
    }
}
