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
use henk_domain::ignore::PathFilter;
use henk_domain::workspace::{
    BackendKind, Change, ChangeKind, EnvLane, FileMode, Limits, Profile, RawChange,
    changeset_refusal,
};

#[cfg(test)]
pub mod contract;
#[cfg(test)]
pub mod fake;
pub mod host;
pub mod setup;
pub mod ssh;
pub mod toolchain;
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

/// What `search` looks for: a regular expression, checked here before any
/// backend sees it. The syntax is what Rust's `regex` and PCRE (`grep -P`,
/// which the `ssh` backend runs) read alike: classes, `\b`, `\d`, `\s`,
/// `\w`, `(?i)`, alternation, repetition. What the two read differently is
/// refused with a reason: class set operations and nested classes (POSIX
/// classes among them), `\<`, `\>`, `\b{...}`, `\D` inside a class and the
/// flags `x`, `R` and `u`. What is left is made to mean the same in both:
/// `\d` is the ASCII digits in both, `$` also matches before the `\r` of a
/// CRLF line, which both see, and PCRE gets `(*UCP)` so `\w`, `\s` and
/// `\b` know letters and spaces beyond ASCII as Rust's do, whatever grep's
/// version.
#[derive(Debug, Clone)]
pub struct Pattern {
    pcre: String,
    regex: regex::Regex,
}

/// Bytes a compiled search pattern may take.
const PATTERN_SIZE: usize = 1 << 20;
/// Characters a search pattern may have.
const PATTERN_LENGTH: usize = 500;

impl Pattern {
    /// Checks and compiles `source`.
    ///
    /// # Errors
    ///
    /// Says, in words for the model, why the pattern is refused.
    pub fn parse(source: &str) -> Result<Self, String> {
        if source.is_empty() {
            return Err("the pattern is empty".to_owned());
        }
        if source.chars().count() > PATTERN_LENGTH {
            return Err(format!(
                "the pattern is longer than {PATTERN_LENGTH} characters"
            ));
        }
        if source.contains('\n') {
            return Err("the pattern spans lines; search matches one line at a time".to_owned());
        }
        let build = |source: &str| {
            regex::RegexBuilder::new(source)
                .size_limit(PATTERN_SIZE)
                .build()
                .map_err(|e| format!("not a regular expression: {e}"))
        };
        // Checked as given first, so an error quotes the model's own pattern.
        build(source)?;
        let common = translate(source)?;
        let regex = build(&common)?;
        Ok(Self {
            pcre: format!("(*UCP){common}"),
            regex,
        })
    }

    /// The pattern as `grep -P` gets it, to match what [`Pattern::is_match`]
    /// does.
    #[must_use]
    pub fn pcre(&self) -> &str {
        &self.pcre
    }

    /// Whether `line` has a match.
    #[must_use]
    pub fn is_match(&self, line: &str) -> bool {
        self.regex.is_match(line)
    }
}

/// `source` in the part of the syntax Rust's `regex` and PCRE read alike,
/// with `\d` and `\D` spelled as ASCII classes, or why it cannot be. Every
/// backslash escapes the one character after it, and a class cannot nest,
/// so one flag says whether the scan is inside `[...]`.
fn translate(source: &str) -> Result<String, String> {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('d') if in_class => out.push_str("0-9"),
                Some('d') => out.push_str("[0-9]"),
                Some('D') if in_class => {
                    return Err(
                        r"\D inside [...] is not supported; use [^0-9] or list the characters"
                            .to_owned(),
                    );
                }
                Some('D') => out.push_str("[^0-9]"),
                Some('<' | '>') => {
                    return Err(
                        r"\< and \> are word boundaries in Rust and plain characters in grep -P; use \b"
                            .to_owned(),
                    );
                }
                Some('b' | 'B') if !in_class && chars.peek() == Some(&'{') => {
                    return Err(
                        r"\b{...} is not supported; use \b, or \b with a \w beside it".to_owned(),
                    );
                }
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            },
            '[' if in_class => {
                return Err(
                    r"a [ inside [...] (a nested class or a POSIX class such as [:alpha:]) is not supported; escape it as \[ or use ranges such as a-z"
                        .to_owned(),
                );
            }
            '[' => {
                in_class = true;
                out.push('[');
                if chars.next_if_eq(&'^').is_some() {
                    out.push('^');
                }
                // A `]` first in a class is the character itself.
                if chars.next_if_eq(&']').is_some() {
                    out.push(']');
                }
            }
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            // A CRLF line ends in `\r` on every backend; `$` is before it.
            '$' if !in_class => out.push_str(r"\r?$"),
            '&' | '-' | '~' if in_class && chars.peek() == Some(&c) => {
                return Err(
                    "class set operations (&&, --, ~~ inside [...]) are not supported; escape the characters or use a simpler class".to_owned(),
                );
            }
            '(' if !in_class && chars.peek() == Some(&'?') => {
                let flags: String = chars
                    .clone()
                    .skip(1)
                    .take_while(|f| "imsUuxR-".contains(*f))
                    .collect();
                if flags.contains(['x', 'R', 'u']) {
                    return Err("of the flags only i, m, s and U are supported".to_owned());
                }
                out.push('(');
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// The lines of `text` as grep reads them: split at `\n` only, so a CRLF
/// line keeps its `\r` on every backend and [`Pattern`] reads `$` as
/// `\r?$` for both engines alike.
pub(crate) fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
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
/// never part of what they see. A backend may leave the checkout's own
/// `.git` in the tree for the run's commands (the `ssh` backend does); the
/// tools never read it and the changeset never carries it.
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

    /// The files under `dir`, sorted, at most `cap`; with `only`, just
    /// the paths it matches, counted before the cap. `.git` is left out and
    /// symbolic links are not followed.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when `dir` is refused or missing.
    async fn list(
        &self,
        dir: &WorkspacePath,
        only: Option<&PathFilter>,
        cap: usize,
    ) -> Result<Vec<String>, WorkspaceError>;

    /// Lines matching `pattern` in the files under `dir` (with `only`, just
    /// the files it matches), sorted by path and line, at most `cap`. Files
    /// larger than `max_file_bytes` or not text are skipped.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when `dir` is refused or missing.
    async fn search(
        &self,
        dir: &WorkspacePath,
        pattern: &Pattern,
        only: Option<&PathFilter>,
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

    /// Makes the tree as it is now the point [`Workspace::export`] counts
    /// from, so what the setup stage left in it is never part of the
    /// changeset (#93). Like `export`, it reads the tree from the backend's
    /// own record and never follows a path in it.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the backend cannot record the tree.
    async fn baseline(&self) -> Result<(), WorkspaceError>;

    /// Destroys the workspace. Calling it again does nothing.
    async fn close(&self);
}

/// Opens workspaces of one backend.
#[async_trait::async_trait]
pub trait WorkspaceProvider: Send + Sync {
    /// Imports the files of the checkout at `source` into a new workspace run
    /// as `profile` says; whether its `.git` comes along is the backend's
    /// choice, since no tool reaches it. The caller may remove `source` once
    /// this returns.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the workspace cannot be made.
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError>;

    /// Removes what a process that died left on a backend that outlives it,
    /// or `None` when the backend keeps nothing. Called once, when Henk
    /// starts: from this call on, a workspace being opened waits until the
    /// sweep is done, so the sweep never takes a live one.
    fn sweep(&self) -> Option<Sweep> {
        None
    }
}

/// A sweep in progress; see [`WorkspaceProvider::sweep`].
pub type Sweep =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), WorkspaceError>> + Send>>;

/// Opens each workspace on the backend its profile names: the host always,
/// the sandbox host when `[workspace.ssh]` is configured.
#[derive(Debug)]
pub struct Backends {
    host: host::HostProvider,
    ssh: Option<ssh::SshProvider>,
}

impl Backends {
    /// The backends there are.
    #[must_use]
    pub fn new(ssh: Option<ssh::SshProvider>) -> Self {
        Self {
            host: host::HostProvider,
            ssh,
        }
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for Backends {
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        match profile.backend {
            BackendKind::Host => self.host.open(source, profile).await,
            BackendKind::Ssh => match &self.ssh {
                Some(ssh) => ssh.open(source, profile).await,
                None => Err(WorkspaceError::Backend(
                    "the ssh backend is not configured: [workspace.ssh] is missing".to_owned(),
                )),
            },
        }
    }

    fn sweep(&self) -> Option<Sweep> {
        self.ssh.as_ref().map(|ssh| Box::pin(ssh.sweep()) as Sweep)
    }
}

/// `workspace` as a run of kind `lane` may use it: a workspace whose
/// changes are never taken out (a review's, #170) refuses `export`, so no
/// tool or later change can turn what a lane did there into a commit
/// (§8.2). An address run's is returned as it is.
#[must_use]
pub fn for_lane(workspace: Arc<dyn Workspace>, lane: EnvLane) -> Arc<dyn Workspace> {
    if lane.exports() {
        workspace
    } else {
        Arc::new(NoExport { inner: workspace })
    }
}

/// A workspace nothing is ever exported from; everything else passes.
struct NoExport {
    inner: Arc<dyn Workspace>,
}

#[async_trait::async_trait]
impl Workspace for NoExport {
    async fn exec(
        &self,
        argv: &[String],
        cwd: &WorkspacePath,
        timeout: Duration,
    ) -> Result<ExecResult, WorkspaceError> {
        self.inner.exec(argv, cwd, timeout).await
    }

    async fn read(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, WorkspaceError> {
        self.inner.read(path, max_bytes).await
    }

    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        self.inner.write(path, content).await
    }

    async fn list(
        &self,
        dir: &WorkspacePath,
        only: Option<&PathFilter>,
        cap: usize,
    ) -> Result<Vec<String>, WorkspaceError> {
        self.inner.list(dir, only, cap).await
    }

    async fn search(
        &self,
        dir: &WorkspacePath,
        pattern: &Pattern,
        only: Option<&PathFilter>,
        max_file_bytes: u64,
        cap: usize,
    ) -> Result<Vec<Hit>, WorkspaceError> {
        self.inner
            .search(dir, pattern, only, max_file_bytes, cap)
            .await
    }

    async fn export(&self) -> Result<Vec<Exported>, WorkspaceError> {
        Err(WorkspaceError::Refused(
            "a review workspace is never exported".to_owned(),
        ))
    }

    async fn baseline(&self) -> Result<(), WorkspaceError> {
        self.inner.baseline().await
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

/// The sandbox host's provider from the settings, when `[workspace.ssh]` is
/// there: its key read from the file the configured variable names.
///
/// # Errors
///
/// Returns an error when the variable is unset or the key or host key does
/// not parse.
pub fn ssh_provider(
    settings: &crate::config::Settings,
) -> anyhow::Result<Option<ssh::SshProvider>> {
    use anyhow::Context as _;
    let Some(config) = &settings.workspace_ssh else {
        return Ok(None);
    };
    let path = crate::app::env_var(&config.key_path_env).ok_or_else(|| {
        anyhow::anyhow!("environment variable {} is not set", config.key_path_env)
    })?;
    let key = russh::keys::load_secret_key(&path, None)
        .with_context(|| format!("reading the sandbox host key {path}"))?;
    let host_key = russh::keys::PublicKey::from_openssh(config.host_key.trim())
        .context("workspace.ssh.host_key")?;
    Ok(Some(ssh::SshProvider::new(ssh::SshTarget {
        host: config.host.clone(),
        port: config.port,
        user: config.user.clone(),
        key: Arc::new(key),
        host_key,
    })))
}

/// What one workspace's commands may still take: each command's own
/// limit, the profile's command limit and what is left of the run's. Shared
/// by every backend, so a used-up run and a stopped command read the same.
#[derive(Debug)]
pub(crate) struct Budget {
    limits: Limits,
    /// Time commands have used so far, against `limits.run_secs`.
    used: std::sync::Mutex<Duration>,
}

impl Budget {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits,
            used: std::sync::Mutex::new(Duration::ZERO),
        }
    }

    /// How long the next command may run when it asks for `asked`.
    pub(crate) fn next(&self, asked: Duration) -> Result<Duration, WorkspaceError> {
        let used = *self
            .used
            .lock()
            .map_err(|_| WorkspaceError::Backend("the workspace is unavailable".to_owned()))?;
        let left = Duration::from_secs(self.limits.run_secs).saturating_sub(used);
        Ok(asked
            .min(Duration::from_secs(self.limits.command_secs))
            .min(left))
    }

    pub(crate) fn spend(&self, spent: Duration) {
        if let Ok(mut used) = self.used.lock() {
            *used += spent;
        }
    }

    /// The bytes of output a command keeps.
    pub(crate) fn output_cap(&self) -> usize {
        usize::try_from(self.limits.output_bytes).unwrap_or(usize::MAX)
    }

    /// The result of a command that was not started: the run's time is gone.
    pub(crate) fn used_up(&self) -> ExecResult {
        ExecResult {
            code: None,
            timed_out: true,
            output: format!(
                "not started: the run's {}s for commands are used up",
                self.limits.run_secs
            ),
            duration: Duration::ZERO,
        }
    }
}

/// One change from `git diff --cached --raw -z --no-abbrev`, and the blob
/// that holds its content when there is one to read: not for a deletion,
/// a link or a submodule.
pub(crate) fn parse_raw_diff(
    raw: &[u8],
) -> Result<Vec<(RawChange, Option<String>)>, WorkspaceError> {
    // `:old new old-sha new-sha status NUL path NUL`, one per change.
    let mut fields = raw
        .split(|b| *b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned());
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        if meta.is_empty() {
            continue;
        }
        let path = fields
            .next()
            .ok_or_else(|| WorkspaceError::Backend("git diff ended early".to_owned()))?;
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (Some(old_mode), Some(new_mode), Some(new_sha), Some(status)) =
            (parts.first(), parts.get(1), parts.get(3), parts.get(4))
        else {
            return Err(WorkspaceError::Backend(format!("git diff said {meta:?}")));
        };
        let deleted = status.starts_with('D');
        let mode = FileMode::from_git(if deleted { old_mode } else { new_mode })
            .ok_or_else(|| WorkspaceError::Backend(format!("unknown mode in {meta:?}")))?;
        let blob = (!deleted && matches!(mode, FileMode::Regular | FileMode::Executable))
            .then(|| (*new_sha).to_owned());
        changes.push((
            RawChange {
                path,
                mode,
                deleted,
            },
            blob,
        ));
    }
    Ok(changes)
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
    fn only_set_operators_inside_a_class_are_refused() {
        for allowed in [
            r"[A-Za-z_]\w*\s*&&",
            r"\[x\] -- y",
            r"[]]&&",
            r"[\]]--",
            r"[a] ~~",
            r"a && b",
        ] {
            assert!(Pattern::parse(allowed).is_ok(), "{allowed}");
        }
        let condition = Pattern::parse(r"[A-Za-z_]\w*\s*&&").unwrap();
        assert!(condition.is_match("if ready && done"));
        for refused in [r"[a&&b]", r"[\w--\d]", r"[^]&&]", r"x [a-c--b]"] {
            let error = Pattern::parse(refused).unwrap_err();
            assert!(error.contains("class set operations"), "{refused}: {error}");
        }
    }

    #[test]
    fn what_the_engines_read_differently_is_refused() {
        for (refused, says) in [
            (r"\<word\>", r"\<"),
            (r"\b{start}x", r"\b{"),
            (r"[a[b]]", "nested"),
            (r"[[:alpha:]]", "POSIX"),
            (r"[^\D]", r"\D inside"),
            (r"(?x) a b", "flags"),
            (r"(?R)a$", "flags"),
            (r"(?-u:\w)", "flags"),
        ] {
            let error = Pattern::parse(refused).unwrap_err();
            assert!(error.contains(says), "{refused}: {error}");
        }
        for allowed in [
            r"(?i)x", r"(?ms)x", r"(?U)a+", r"(?:a|b)", r"(?i:a)b", r"\b\w+\b", r"[<>]",
        ] {
            assert!(Pattern::parse(allowed).is_ok(), "{allowed}");
        }
    }

    #[test]
    fn digits_are_ascii_and_an_escaped_backslash_stays_one() {
        let digit = Pattern::parse(r"^\d$").unwrap();
        assert!(digit.is_match("7"));
        assert!(!digit.is_match("\u{663}"));
        assert!(Pattern::parse(r"^[\d_]+$").unwrap().is_match("7_"));
        assert_eq!(
            Pattern::parse(r"[\d_]\D").unwrap().pcre(),
            "(*UCP)[0-9_][^0-9]"
        );
        let literal = Pattern::parse(r"^\\d$").unwrap();
        assert!(literal.is_match(r"\d"));
        assert!(!literal.is_match("7"));
        assert_eq!(Pattern::parse(r"\\d").unwrap().pcre(), r"(*UCP)\\d");
        let refused = Pattern::parse(r"(\d").unwrap_err();
        assert!(refused.contains(r"(\d"), "{refused}");
    }

    #[tokio::test]
    async fn a_review_workspace_is_never_exported_but_works_otherwise() {
        let source = crate::git::ScratchDir::new("review-no-export").unwrap();
        std::fs::write(source.path().join("a.txt"), "one\n").unwrap();
        let provider = fake::FakeProvider::default();
        let opened = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        let review = for_lane(Arc::clone(&opened), EnvLane::Review);
        let path = WorkspacePath::parse("a.txt").unwrap();
        review.write(&path, b"two\n").await.unwrap();
        assert_eq!(review.read(&path, 100).await.unwrap(), b"two\n");
        let refused = review.export().await.unwrap_err();
        assert!(refused.to_string().contains("never exported"), "{refused}");
        assert_eq!(
            opened.export().await.unwrap().len(),
            1,
            "the change is there"
        );
        let address = for_lane(Arc::clone(&opened), EnvLane::Address);
        assert_eq!(address.export().await.unwrap().len(), 1);
        review.close().await;
        assert_eq!(provider.live(), 0);
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
