//! What every backend that runs Henk's sandbox script has in common (#84,
//! #89): the requests, the workspace built on them, and the script itself
//! (`sandbox.sh`), sent with every request so nothing of Henk's is
//! installed where it runs.
//!
//! A workspace here is a checkout, `.git` and all, in `work` under a home of
//! its own, with a record of the checkout kept beside it. Every file and
//! command goes through fixed scripts run as the workspace's user, their
//! arguments as `b`+base64 tokens, so nothing a model wrote is ever
//! evaluated by a shell. How a request reaches the script is the backend's:
//! an SSH channel to a sandbox host as root, where each workspace is a user
//! of its own and the record only root can write (`ssh.rs`), or `pods/exec`
//! into a Pod that is the workspace, where the Pod itself is the boundary
//! (`kubernetes.rs`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use henk_domain::address::WorkspacePath;
use henk_domain::ignore::PathFilter;
use henk_domain::workspace::Profile;
use tracing::warn;

use super::{
    Budget, ExecResult, Exported, Hit, Pattern, Workspace, WorkspaceError, parse_raw_diff, tail,
};

/// How long a request that is not a command may take: making the user,
/// importing the checkout, reading the record, a file operation.
pub(crate) const REQUEST_WAIT: Duration = Duration::from_mins(5);

/// What the runner's own `timeout` adds before it kills, plus slack for the
/// connection: past this, Henk stops waiting.
pub(crate) const COMMAND_SLACK: Duration = Duration::from_secs(15);

/// The most a reply may hold when it is kept whole (a file, a listing, a
/// diff). The callers ask for far less.
pub(crate) const MAX_REPLY: usize = 64 * 1024 * 1024;

/// What the runner sent back.
#[derive(Debug, Default)]
pub(crate) struct Reply {
    /// Exit status; `None` when the runner ended on a signal or did not say.
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// Henk stopped waiting before the runner finished.
    pub(crate) gave_up: bool,
}

impl Reply {
    pub(crate) fn ok(self, what: &str) -> Result<Self, WorkspaceError> {
        if self.code == Some(0) {
            Ok(self)
        } else {
            Err(WorkspaceError::Backend(format!(
                "{what} failed: {}",
                String::from_utf8_lossy(&self.stderr).trim()
            )))
        }
    }
}

/// Carries requests to the sandbox script: over SSH, through `pods/exec`,
/// or for tests to the script run here.
#[async_trait::async_trait]
pub(crate) trait Runner: Send + Sync {
    /// Sends `tokens` (the subcommand first) with `stdin` and waits at most
    /// `wait`. With `keep`, only the last `keep` bytes of each stream are
    /// kept while it arrives; without, a stream is kept whole up to
    /// [`MAX_REPLY`].
    async fn call(
        &self,
        tokens: &[String],
        stdin: &[u8],
        keep: Option<usize>,
        wait: Duration,
    ) -> Result<Reply, WorkspaceError>;

    /// Removes workspace `id` and everything it left. The script's
    /// `destroy` by default; a backend whose workspace is a machine of its
    /// own, such as a Pod, removes that instead.
    async fn release(&self, id: &str) -> Result<(), WorkspaceError> {
        self.call(
            &["destroy".to_owned(), id.to_owned()],
            &[],
            None,
            REQUEST_WAIT,
        )
        .await?
        .ok("removing the workspace")
        .map(|_| ())
    }
}

/// The end of a stream, as much of it as is kept.
pub(crate) struct Kept {
    keep: Option<usize>,
    bytes: Vec<u8>,
}

impl Kept {
    pub(crate) fn new(keep: Option<usize>) -> Self {
        Self {
            keep,
            bytes: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<(), WorkspaceError> {
        self.bytes.extend_from_slice(data);
        match self.keep {
            // Trimmed in steps, so a long stream is not copied per chunk.
            Some(keep) if self.bytes.len() > keep.saturating_mul(2).max(64 * 1024) => {
                let cut = self.bytes.len() - keep;
                self.bytes.drain(..cut);
            }
            None if self.bytes.len() > MAX_REPLY => {
                return Err(WorkspaceError::Backend(format!(
                    "the workspace answered more than {MAX_REPLY} bytes"
                )));
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        if let Some(keep) = self.keep
            && self.bytes.len() > keep
        {
            let cut = self.bytes.len() - keep;
            self.bytes.drain(..cut);
        }
        self.bytes
    }
}

/// `b` and the base64 of `text`: the only form a path or argument takes in
/// a request. The `b` keeps an empty value a token when the request is
/// split on spaces.
pub(crate) fn token(text: &str) -> String {
    format!("b{}", STANDARD.encode(text))
}

/// The sandbox script, sent with every request: nothing of Henk's is
/// installed on the host.
pub(crate) const SCRIPT: &str = include_str!("sandbox.sh");

/// Refuses a request with a token outside `A-Z a-z 0-9 + / = _ . -`
/// before anything is sent: the script splits on nothing else, and a shell
/// on the way evaluates none of these.
///
/// # Errors
///
/// Names the first token refused.
pub(crate) fn check_tokens(tokens: &[String]) -> Result<(), WorkspaceError> {
    let safe = |t: &String| {
        !t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=_.-".contains(&b))
    };
    match tokens.iter().find(|t| !safe(t)) {
        Some(bad) => Err(WorkspaceError::Backend(format!(
            "refused to send a request with the token {bad:?}"
        ))),
        None => Ok(()),
    }
}

/// The shell line that runs one request on the host: the script, quoted,
/// then its tokens, which may only hold `A-Z a-z 0-9 + / = _ . -`, so the
/// shell takes each as one word and evaluates none. Signed in as anyone but
/// root, the line runs through `sudo -n`.
///
/// # Errors
///
/// Refuses a token with any other character before anything is sent.
pub(crate) fn command_line(user: &str, tokens: &[String]) -> Result<String, WorkspaceError> {
    check_tokens(tokens)?;
    let quoted = SCRIPT.replace('\'', "'\\''");
    let sudo = if user == "root" { "" } else { "sudo -n " };
    Ok(format!(
        "{sudo}sh -c '{quoted}' henk-sandbox {}",
        tokens.join(" ")
    ))
}

/// A new workspace id: `w` and twelve hex digits.
pub(crate) fn new_workspace_id() -> String {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0_u8; 6];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        bytes = nanos
            .to_le_bytes()
            .get(..6)
            .and_then(|b| b.try_into().ok())
            .unwrap_or([0; 6]);
    }
    bytes.iter().fold("w".to_owned(), |mut id, b| {
        use std::fmt::Write as _;
        let _ = write!(id, "{b:02x}");
        id
    })
}

/// The checkout at `source` as a tar, `.git` included, links kept as links.
pub(crate) fn pack(source: &Path) -> Result<Vec<u8>, WorkspaceError> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    builder
        .append_dir_all(".", source)
        .and_then(|()| builder.into_inner())
        .map_err(|e| WorkspaceError::Backend(format!("cannot pack the checkout: {e}")))
}

/// What a path in the tree is, as the run's user sees it.
#[derive(Debug, PartialEq, Eq)]
struct Resolved {
    /// The path itself is a symbolic link.
    link: bool,
    /// What it is after following links: `file`, `dir`, `other` or `missing`.
    kind: String,
    /// Its real path, when it exists.
    real: Option<String>,
    /// The real path of the nearest existing directory above it.
    ancestor: String,
}

/// Prints what `$1` is, NUL-separated: link or not, the kind after links,
/// its real path, and the real path of the nearest existing directory above
/// it, where a write would make what is missing.
const RESOLVE: &str = r#"t=$1
if [ -L "$t" ]; then l=1; else l=0; fi
if [ -d "$t" ]; then k=dir; elif [ -f "$t" ]; then k=file; elif [ -e "$t" ]; then k=other; else k=missing; fi
r=$(realpath -e -- "$t" 2>/dev/null) || r=
a=$(dirname -- "$t")
while [ ! -e "$a" ] && [ ! -L "$a" ]; do a=$(dirname -- "$a"); done
ra=$(realpath -e -- "$a" 2>/dev/null) || ra=
printf '%s\0%s\0%s\0%s\0' "$l" "$k" "$r" "$ra""#;

/// Prints the size of `$1`, then exits 3 when it is over `$2` bytes, or its
/// content after a NUL.
const READ: &str = r#"s=$(stat -c %s -- "$1") || exit 2
if [ "$s" -gt "$2" ]; then exit 3; fi
cat -- "$1""#;

/// Writes stdin to `$1`, making its directories; an existing file keeps its
/// mode and a new one is not executable (the runner's umask is 022).
const WRITE: &str = r#"mkdir -p -- "$(dirname -- "$1")" && cat > "$1""#;

/// The regular files under `$1`, NUL-separated, links not followed and any
/// `.git` directory left out.
const LIST: &str = r#"find -P "$1" \( -iname .git -prune \) -o \( -type f -print0 \)"#;

/// The regular files under `$1` of fewer than `$2` bytes, NUL-separated,
/// links not followed and any `.git` directory left out: what `search`
/// would read, for Henk to narrow by a glob first.
const SEARCHED: &str =
    r#"find -P "$1" \( -iname .git -prune \) -o \( -type f -size -"$2"c -print0 \)"#;

/// Lines matching `$3`, a [`Pattern`] read by PCRE (`grep -P`), as
/// `path NUL line:text`, with `$4` lines of context around each as
/// `path NUL line-text` and `--` between blocks, in the text files under `$1`
/// of fewer than `$2` bytes, or with `$1` `-` in the NUL-separated files on stdin, so a glob
/// keeps the others from grep altogether. Files the run's user cannot read
/// are left out first, so a grep that says 2 failed on the pattern itself:
/// no `-P`, a pattern PCRE refuses or its backtracking limit. Each grep's
/// 1, no match, becomes 0 and its 2 becomes 255, on which xargs stops and
/// says 124, so that is an error and never "no matches".
const SEARCH: &str = r#"if [ "$1" = - ]; then cat; else
find -P "$1" \( -iname .git -prune \) -o \( -type f -size -"$2"c -print0 \); fi |
xargs -0 -r sh -c 'p=$1
c=$2
shift 2
for f; do
    shift
    if [ -r "$f" ]; then set -- "$@" "$f"; fi
done
[ "$#" -eq 0 ] || grep -HIPn --null -C "$c" -e "$p" -- "$@" || [ "$?" -eq 1 ] || exit 255' sh "$3" "$4""#;

/// `limit` in seconds for the runner's `timeout`, to the millisecond as the
/// host backend's limit is, rounded up: `timeout` reads 0 as no limit at all,
/// so what is left of a run is never sent as `0.000`.
pub(crate) fn seconds(limit: Duration) -> String {
    let millis = limit.as_nanos().div_ceil(1_000_000).max(1);
    format!("{}.{:03}", millis / 1000, millis % 1000)
}

/// A workspace the sandbox script keeps: on a sandbox host or in a Pod.
pub struct RemoteWorkspace {
    runner: Arc<dyn Runner>,
    id: String,
    /// The tree's absolute path on the host.
    work: String,
    budget: Budget,
    closed: AtomicBool,
}

impl std::fmt::Debug for RemoteWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteWorkspace")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl RemoteWorkspace {
    fn alive(&self) -> Result<(), WorkspaceError> {
        if self.closed.load(Ordering::Acquire) {
            Err(WorkspaceError::Backend(
                "the workspace is closed".to_owned(),
            ))
        } else {
            Ok(())
        }
    }

    /// Runs a fixed script of Henk's as the run's user, in the tree's root,
    /// its arguments passed as `$1`… so nothing in them is ever evaluated.
    async fn script(
        &self,
        script: &str,
        args: &[&str],
        stdin: &[u8],
    ) -> Result<Reply, WorkspaceError> {
        self.alive()?;
        let mut tokens = vec![
            "as".to_owned(),
            self.id.clone(),
            REQUEST_WAIT.as_secs().to_string(),
            token(""),
            token("sh"),
            token("-c"),
            token(script),
            token("sh"),
        ];
        tokens.extend(args.iter().map(|a| token(a)));
        let reply = self
            .runner
            .call(&tokens, stdin, None, REQUEST_WAIT + COMMAND_SLACK)
            .await?;
        if reply.gave_up {
            return Err(WorkspaceError::Backend(
                "the sandbox host did not answer in time".to_owned(),
            ));
        }
        Ok(reply)
    }

    async fn resolve(&self, path: &WorkspacePath) -> Result<Resolved, WorkspaceError> {
        let target = if path.as_str().is_empty() {
            "."
        } else {
            path.as_str()
        };
        let reply = self
            .script(RESOLVE, &[target], &[])
            .await?
            .ok("looking up a path")?;
        let mut fields = reply.stdout.split(|b| *b == 0);
        let mut next = || {
            fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned())
        };
        let (Some(link), Some(kind), Some(real), Some(ancestor)) = (next(), next(), next(), next())
        else {
            return Err(WorkspaceError::Backend(format!("cannot look up {path}")));
        };
        Ok(Resolved {
            link: link == "1",
            kind,
            real: (!real.is_empty()).then_some(real),
            ancestor,
        })
    }

    /// Where `real` (a real path on the host) is in the tree, or why it is
    /// refused; the same rules and words as the host backend.
    fn inside(&self, real: &str, shown: &WorkspacePath) -> Result<WorkspacePath, WorkspaceError> {
        let rest = if real == self.work {
            ""
        } else {
            real.strip_prefix(&self.work)
                .and_then(|r| r.strip_prefix('/'))
                .ok_or_else(|| {
                    WorkspaceError::Refused(format!("{shown} leads out of the repository"))
                })?
        };
        WorkspacePath::parse_dir(rest)
            .map_err(|_| WorkspaceError::Refused(format!("{shown} leads into .git")))
    }

    /// The real path of an existing `path` in the tree.
    async fn existing(&self, path: &WorkspacePath) -> Result<(Resolved, String), WorkspaceError> {
        let resolved = self.resolve(path).await?;
        let real = resolved
            .real
            .clone()
            .ok_or_else(|| WorkspaceError::NotFound(path.to_string()))?;
        self.inside(&real, path)?;
        Ok((resolved, real))
    }

    /// The tree-relative path of a path the host printed, when it is in the
    /// tree and not in `.git`.
    fn relative(&self, real: &str) -> Option<String> {
        let rest = real.strip_prefix(&self.work)?.strip_prefix('/')?;
        WorkspacePath::parse(rest)
            .ok()
            .map(|p| p.as_str().to_owned())
    }
}

#[async_trait::async_trait]
impl Workspace for RemoteWorkspace {
    async fn exec(
        &self,
        argv: &[String],
        cwd: &WorkspacePath,
        timeout: Duration,
    ) -> Result<ExecResult, WorkspaceError> {
        if argv.is_empty() {
            return Err(WorkspaceError::Refused("an empty command".to_owned()));
        }
        let (resolved, real) = self.existing(cwd).await?;
        if resolved.kind != "dir" {
            return Err(WorkspaceError::Refused(format!("{cwd} is not a directory")));
        }
        let dir = self.inside(&real, cwd)?;
        let held = self.budget.reserve(timeout)?;
        let limit = held.granted();
        if limit.is_zero() {
            return Ok(self.budget.used_up());
        }
        let mut tokens = vec![
            "as".to_owned(),
            self.id.clone(),
            seconds(limit),
            token(dir.as_str()),
        ];
        tokens.extend(argv.iter().map(|a| token(a)));
        let cap = self.budget.output_cap();
        let started = Instant::now();
        let reply = self
            .runner
            // One byte over the cap, so `tail` sees that something was cut.
            .call(&tokens, &[], Some(cap.saturating_add(1)), limit + COMMAND_SLACK)
            .await?;
        let duration = started.elapsed();
        // The runner's `timeout` ends a command that ran out with 124, or
        // 137 when it had to kill it.
        let timed_out = reply.gave_up
            || (matches!(reply.code, Some(124 | 137))
                && duration + Duration::from_secs(1) >= limit);
        let mut text = String::from_utf8_lossy(&reply.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&reply.stderr));
        if timed_out {
            text = format!("{}stopped after {}s", tail(&text, cap), limit.as_secs());
        }
        let result = ExecResult {
            code: if timed_out { None } else { reply.code },
            timed_out,
            output: tail(&text, cap),
            duration,
        };
        held.settle(result.duration);
        Ok(result)
    }

    async fn read(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, WorkspaceError> {
        let (resolved, real) = self.existing(path).await?;
        if resolved.kind != "file" {
            return Err(WorkspaceError::Refused(format!("{path} is not a file")));
        }
        let reply = self
            .script(READ, &[&real, &max_bytes.to_string()], &[])
            .await?;
        match reply.code {
            Some(0) => Ok(reply.stdout),
            Some(3) => Err(WorkspaceError::Refused(format!(
                "{path} is larger than {max_bytes} bytes"
            ))),
            _ => Err(WorkspaceError::Backend(format!(
                "reading {path} failed: {}",
                String::from_utf8_lossy(&reply.stderr).trim()
            ))),
        }
    }

    async fn write(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        let resolved = self.resolve(path).await?;
        if resolved.link {
            return Err(WorkspaceError::Refused(format!(
                "{path} is a symbolic link; edit what it points to"
            )));
        }
        if resolved.kind == "dir" {
            return Err(WorkspaceError::Refused(format!("{path} is a directory")));
        }
        self.inside(&resolved.ancestor, path)?;
        let ancestor = self.resolve_real_dir(&resolved.ancestor, path).await?;
        if !ancestor {
            return Err(WorkspaceError::Refused(format!(
                "{path} is under a file, not a directory"
            )));
        }
        let target = format!("{}/{}", self.work, path.as_str());
        self.script(WRITE, &[&target], content)
            .await?
            .ok(&format!("writing {path}"))
            .map(|_| ())
    }

    async fn list(
        &self,
        dir: &WorkspacePath,
        only: Option<&PathFilter>,
        cap: usize,
    ) -> Result<Vec<String>, WorkspaceError> {
        let (_, real) = self.existing(dir).await?;
        let reply = self
            .script(LIST, &[&real], &[])
            .await?
            .ok("listing files")?;
        let mut files: Vec<String> = reply
            .stdout
            .split(|b| *b == 0)
            .filter_map(|f| std::str::from_utf8(f).ok())
            .filter_map(|f| self.relative(f))
            .filter(|f| only.is_none_or(|o| o.matches(f)))
            .collect();
        files.sort();
        files.truncate(cap);
        Ok(files)
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
        let (_, real) = self.existing(dir).await?;
        let under = max_file_bytes.saturating_add(1).to_string();
        // With a glob, the files are narrowed here and sent on stdin, so
        // grep never reads one the glob leaves out.
        let mut files = Vec::new();
        if let Some(only) = only {
            let found = self
                .script(SEARCHED, &[&real, &under], &[])
                .await?
                .ok("listing files")?;
            for file in found.stdout.split(|b| *b == 0) {
                let kept = std::str::from_utf8(file)
                    .ok()
                    .and_then(|f| self.relative(f))
                    .is_some_and(|f| only.matches(&f));
                if kept {
                    files.extend_from_slice(file);
                    files.push(0);
                }
            }
            if files.is_empty() {
                return Ok(Vec::new());
            }
        }
        let from = if only.is_some() { "-" } else { real.as_str() };
        let reply = self
            .script(
                SEARCH,
                &[from, &under, pattern.pcre(), &context.to_string()],
                &files,
            )
            .await?
            .ok("searching")?;
        let mut hits = Vec::new();
        // `path NUL line:text NL` for a match and `path NUL line-text NL`
        // for context, repeated, with `--` lines between blocks.
        let mut rest = reply.stdout.as_slice();
        loop {
            while let Some(after) = rest.strip_prefix(b"--\n") {
                rest = after;
            }
            let Some(at) = rest.iter().position(|b| *b == 0) else {
                break;
            };
            let (file, after) = rest.split_at(at);
            let after = after.get(1..).unwrap_or_default();
            let end = after
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(after.len());
            let (line, next) = after.split_at(end);
            rest = next.get(1..).unwrap_or_default();
            let (Ok(file), Ok(line)) = (std::str::from_utf8(file), std::str::from_utf8(line))
            else {
                continue;
            };
            let digits = line.bytes().take_while(u8::is_ascii_digit).count();
            let (Some(path), Some(number), Some(mark), Some(text)) = (
                self.relative(file),
                line.get(..digits).and_then(|n| n.parse().ok()),
                line.get(digits..=digits),
                line.get(digits + 1..),
            ) else {
                continue;
            };
            if only.is_some_and(|o| !o.matches(&path)) {
                continue;
            }
            hits.push(Hit {
                path,
                line: number,
                text: text.strip_suffix('\r').unwrap_or(text).to_owned(),
                matched: mark == ":",
            });
        }
        hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        hits.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        Ok(super::keep_matches(hits, cap, context))
    }

    async fn export(&self) -> Result<Vec<Exported>, WorkspaceError> {
        self.alive()?;
        let request = |what: &str| vec!["record".to_owned(), self.id.clone(), what.to_owned()];
        let diff = self
            .runner
            .call(&request("diff"), &[], None, REQUEST_WAIT)
            .await?
            .ok("reading the changes")?;
        let mut changes = Vec::new();
        for (raw, blob) in parse_raw_diff(&diff.stdout)? {
            let content = match blob {
                Some(sha) => {
                    let mut tokens = request("blob");
                    tokens.push(sha);
                    self.runner
                        .call(&tokens, &[], None, REQUEST_WAIT)
                        .await?
                        .ok("reading a changed file")?
                        .stdout
                }
                None => Vec::new(),
            };
            changes.push(Exported { raw, content });
        }
        Ok(changes)
    }

    async fn baseline(&self) -> Result<(), WorkspaceError> {
        self.alive()?;
        self.runner
            .call(
                &["record".to_owned(), self.id.clone(), "baseline".to_owned()],
                &[],
                None,
                REQUEST_WAIT,
            )
            .await?
            .ok("recording the tree after setup")?;
        Ok(())
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(error) = self.runner.release(&self.id).await {
            warn!(%error, workspace = %self.id, "a workspace was kept; the next sweep removes it");
        }
    }
}

impl RemoteWorkspace {
    /// Whether the real path `real` (an ancestor of `path`) is a directory.
    async fn resolve_real_dir(
        &self,
        real: &str,
        path: &WorkspacePath,
    ) -> Result<bool, WorkspaceError> {
        let relative = self.inside(real, path)?;
        let resolved = self.resolve(&relative).await?;
        Ok(resolved.kind == "dir")
    }
}

impl Drop for RemoteWorkspace {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // A run that was dropped instead of closed still leaves nothing: the
        // removal runs on its own, and the next sweep catches what it misses.
        let (runner, id) = (Arc::clone(&self.runner), self.id.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = runner.release(&id).await;
            });
        }
    }
}

impl RemoteWorkspace {
    /// A new workspace holding the checkout `archive` (from [`pack`]),
    /// made by the script through `runner`. It exists before the script
    /// makes it, so any failure from here on drops it, which removes
    /// whatever was made: a `create` that failed half-way, or an import.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the workspace cannot be made or the
    /// checkout cannot be imported.
    pub(crate) async fn start(
        runner: Arc<dyn Runner>,
        id: String,
        profile: &Profile,
        archive: &[u8],
    ) -> Result<Self, WorkspaceError> {
        let mut workspace = Self {
            runner,
            id,
            work: String::new(),
            budget: Budget::new(profile.limits.clone()),
            closed: AtomicBool::new(false),
        };
        let created = workspace
            .runner
            .call(
                &["create".to_owned(), workspace.id.clone()],
                &[],
                None,
                REQUEST_WAIT,
            )
            .await?
            .ok("making the workspace")?;
        String::from_utf8_lossy(&created.stdout)
            .trim()
            .clone_into(&mut workspace.work);
        workspace
            .runner
            .call(
                &["import".to_owned(), workspace.id.clone()],
                archive,
                None,
                REQUEST_WAIT,
            )
            .await?
            .ok("importing the checkout")?;
        Ok(workspace)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::path::PathBuf;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::git::ScratchDir;

    /// The sandbox script, run here as the current user
    /// in its test mode: a workspace is a directory and nothing changes
    /// user. It checks the script and the backend together without SSH.
    pub(crate) struct LocalRunner {
        _scratch: ScratchDir,
        base: PathBuf,
    }

    impl LocalRunner {
        pub(crate) fn new(name: &str) -> Arc<Self> {
            let scratch = ScratchDir::new(name).unwrap();
            let base = scratch.path().to_owned();
            Arc::new(Self {
                _scratch: scratch,
                base,
            })
        }

        /// A runner whose workspaces sit behind a symbolic link, as on a
        /// host where /home links to /var/home.
        pub(crate) fn behind_a_link(name: &str) -> Arc<Self> {
            let scratch = ScratchDir::new(name).unwrap();
            std::fs::create_dir(scratch.path().join("real")).unwrap();
            std::os::unix::fs::symlink("real", scratch.path().join("home")).unwrap();
            let base = scratch.path().join("home");
            Arc::new(Self {
                _scratch: scratch,
                base,
            })
        }

        pub(crate) fn base(&self) -> &Path {
            &self.base
        }
    }

    #[async_trait::async_trait]
    impl Runner for LocalRunner {
        async fn call(
            &self,
            tokens: &[String],
            stdin: &[u8],
            keep: Option<usize>,
            wait: Duration,
        ) -> Result<Reply, WorkspaceError> {
            // The very line the host gets, so its quoting is tested too.
            let line = command_line("root", tokens)?;
            let mut child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(line)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HENK_SANDBOX_BASE", &self.base)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = child.stdin.take().unwrap();
            let stdin = stdin.to_vec();
            tokio::spawn(async move {
                let _ = input.write_all(&stdin).await;
            });
            let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
            let run = async {
                let (mut o, mut e) = (Vec::new(), Vec::new());
                let (r1, r2) = tokio::join!(out.read_to_end(&mut o), err.read_to_end(&mut e));
                r1.unwrap();
                r2.unwrap();
                let status = child.wait().await.unwrap();
                (status.code(), o, e)
            };
            match tokio::time::timeout(wait, run).await {
                Ok((code, o, e)) => {
                    let (mut so, mut se) = (Kept::new(keep), Kept::new(keep));
                    so.push(&o)?;
                    se.push(&e)?;
                    Ok(Reply {
                        code,
                        stdout: so.finish(),
                        stderr: se.finish(),
                        gave_up: false,
                    })
                }
                Err(_) => Ok(Reply {
                    gave_up: true,
                    ..Reply::default()
                }),
            }
        }
    }
}
