//! The ssh backend (#84): a workspace on a sandbox host, reached over SSH.
//!
//! Each workspace is a throwaway user on that host with its own checkout of
//! the pull request, `.git` and all, in `~/work`. Henk's key may run one
//! program there, `henk-runner` (`deploy/sandbox/henk-runner`), which makes
//! the user, runs commands as it, keeps a record of the checkout that only
//! root can write, and removes the user again. Every file and command the
//! run touches goes through that user, so the run reaches nothing on the
//! host its user cannot. The changeset comes from the record, never from
//! the tree's own `.git`.
//!
//! Runs share the host's kernel, `/tmp` and network, and memory, cpu, pids
//! and disk are not bounded; `henk config check` says so. The SSH key and
//! the platform token stay with Henk (§8.4): the checkout is copied in, not
//! cloned there.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use henk_domain::address::WorkspacePath;
use henk_domain::ignore::PathFilter;
use henk_domain::workspace::Profile;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelMsg, client};
use tracing::warn;

use super::{
    Budget, ExecResult, Exported, Hit, Pattern, Workspace, WorkspaceError, WorkspaceProvider,
    parse_raw_diff, tail,
};

/// How long a request that is not a command may take: making the user,
/// importing the checkout, reading the record, a file operation.
const REQUEST_WAIT: Duration = Duration::from_mins(5);

/// How long making the connection and signing in may take. The connection
/// is shared, so a host that accepts and then says nothing must not hold
/// every other request up for longer than this.
const CONNECT_WAIT: Duration = Duration::from_secs(30);

/// What the runner's own `timeout` adds before it kills, plus slack for the
/// connection: past this, Henk stops waiting.
const COMMAND_SLACK: Duration = Duration::from_secs(15);

/// The most a reply may hold when it is kept whole (a file, a listing, a
/// diff). The callers ask for far less.
const MAX_REPLY: usize = 64 * 1024 * 1024;

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
    fn ok(self, what: &str) -> Result<Self, WorkspaceError> {
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

/// Carries requests to `henk-runner`: over SSH, or for tests to a local copy
/// of the script.
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
}

/// The end of a stream, as much of it as is kept.
struct Kept {
    keep: Option<usize>,
    bytes: Vec<u8>,
}

impl Kept {
    fn new(keep: Option<usize>) -> Self {
        Self {
            keep,
            bytes: Vec::new(),
        }
    }

    fn push(&mut self, data: &[u8]) -> Result<(), WorkspaceError> {
        self.bytes.extend_from_slice(data);
        match self.keep {
            // Trimmed in steps, so a long stream is not copied per chunk.
            Some(keep) if self.bytes.len() > keep.saturating_mul(2).max(64 * 1024) => {
                let cut = self.bytes.len() - keep;
                self.bytes.drain(..cut);
            }
            None if self.bytes.len() > MAX_REPLY => {
                return Err(WorkspaceError::Backend(format!(
                    "the sandbox host answered more than {MAX_REPLY} bytes"
                )));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        if let Some(keep) = self.keep
            && self.bytes.len() > keep
        {
            let cut = self.bytes.len() - keep;
            self.bytes.drain(..cut);
        }
        self.bytes
    }
}

/// Where the sandbox host is and how Henk proves who he is and checks who
/// it is.
#[derive(Clone)]
pub struct SshTarget {
    /// Host name or address.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// The user whose key runs `henk-runner`.
    pub user: String,
    /// Henk's private key. It never leaves Henk.
    pub key: Arc<PrivateKey>,
    /// The host key Henk expects; anything else is refused.
    pub host_key: PublicKey,
}

impl std::fmt::Debug for SshTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Whether the key the server offered is the pinned one. A certificate is
/// never accepted in its place: there is no trust on first use and no CA.
pub(crate) fn host_key_matches(
    pinned: &PublicKey,
    offered: &russh::keys::PublicKeyOrCertificate,
) -> bool {
    match offered {
        russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => {
            key.key_data() == pinned.key_data()
        }
        russh::keys::PublicKeyOrCertificate::Certificate(_) => false,
    }
}

/// The client side of one connection: it only decides on the host key.
struct Client {
    host_key: PublicKey,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        offered: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(host_key_matches(&self.host_key, offered))
    }
}

/// The runner over one SSH connection, opened on first use and opened again
/// after it fails. Every request is a channel of its own, so runs share it.
pub(crate) struct SshRunner {
    target: SshTarget,
    connection: tokio::sync::Mutex<Option<Arc<client::Handle<Client>>>>,
    /// At most [`CONNECT_WAIT`]; shorter in tests.
    connect_wait: Duration,
}

impl SshRunner {
    pub(crate) fn new(target: SshTarget) -> Self {
        Self {
            target,
            connection: tokio::sync::Mutex::new(None),
            connect_wait: CONNECT_WAIT,
        }
    }

    async fn connect(&self) -> Result<Arc<client::Handle<Client>>, WorkspaceError> {
        let mut slot = self.connection.lock().await;
        if let Some(handle) = slot.as_ref()
            && !handle.is_closed()
        {
            return Ok(Arc::clone(handle));
        }
        let handle = tokio::time::timeout(self.connect_wait, self.sign_in())
            .await
            .map_err(|_| {
                WorkspaceError::Backend(format!(
                    "the sandbox host {}:{} did not answer within {}s",
                    self.target.host,
                    self.target.port,
                    self.connect_wait.as_secs()
                ))
            })??;
        let handle = Arc::new(handle);
        *slot = Some(Arc::clone(&handle));
        Ok(handle)
    }

    /// A new connection, its host key checked and Henk signed in.
    async fn sign_in(&self) -> Result<client::Handle<Client>, WorkspaceError> {
        let failed = |what: &str, error: &dyn std::fmt::Display| {
            WorkspaceError::Backend(format!(
                "{what} {}:{} failed: {error}",
                self.target.host, self.target.port
            ))
        };
        let config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_secs(30)),
            ..client::Config::default()
        });
        let client = Client {
            host_key: self.target.host_key.clone(),
        };
        let mut handle = client::connect(
            config,
            (self.target.host.as_str(), self.target.port),
            client,
        )
        .await
        .map_err(|e| match e {
            russh::Error::UnknownKey => WorkspaceError::Backend(format!(
                "{}:{} offered a host key that is not the pinned workspace.ssh.host_key",
                self.target.host, self.target.port
            )),
            other => failed("connecting to the sandbox host", &other),
        })?;
        let key = PrivateKeyWithHashAlg::new(Arc::clone(&self.target.key), None);
        let auth = handle
            .authenticate_publickey(self.target.user.clone(), key)
            .await
            .map_err(|e| failed("signing in to the sandbox host", &e))?;
        if !auth.success() {
            return Err(WorkspaceError::Backend(format!(
                "the sandbox host {} did not accept Henk's key for {}",
                self.target.host, self.target.user
            )));
        }
        Ok(handle)
    }

    async fn forget(&self) {
        *self.connection.lock().await = None;
    }
}

#[async_trait::async_trait]
impl Runner for SshRunner {
    async fn call(
        &self,
        tokens: &[String],
        stdin: &[u8],
        keep: Option<usize>,
        wait: Duration,
    ) -> Result<Reply, WorkspaceError> {
        let handle = self.connect().await?;
        let lost = |error: russh::Error| {
            WorkspaceError::Backend(format!(
                "the connection to the sandbox host failed: {error}"
            ))
        };
        let exchange = async {
            let mut channel = handle.channel_open_session().await.map_err(lost)?;
            channel.exec(true, tokens.join(" ")).await.map_err(lost)?;
            if !stdin.is_empty() {
                channel.data(stdin).await.map_err(lost)?;
            }
            channel.eof().await.map_err(lost)?;
            let (mut out, mut err) = (Kept::new(keep), Kept::new(keep));
            let mut code = None;
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Data { data } => out.push(&data)?,
                    ChannelMsg::ExtendedData { data, ext: 1 } => err.push(&data)?,
                    ChannelMsg::ExitStatus { exit_status } => {
                        code = i32::try_from(exit_status).ok();
                    }
                    ChannelMsg::Failure => {
                        return Err(WorkspaceError::Backend(
                            "the sandbox host refused the request".to_owned(),
                        ));
                    }
                    _ => {}
                }
            }
            Ok(Reply {
                code,
                stdout: out.finish(),
                stderr: err.finish(),
                gave_up: false,
            })
        };
        match tokio::time::timeout(wait, exchange).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => {
                // A dropped connection fails this request and the next one
                // connects again; the run fails before anything is pushed.
                if handle.is_closed() {
                    self.forget().await;
                }
                Err(error)
            }
            Err(_) => Ok(Reply {
                gave_up: true,
                ..Reply::default()
            }),
        }
    }
}

/// `b` and the base64 of `text`: the only form a path or argument takes in
/// a request. The `b` keeps an empty value a token when the request is
/// split on spaces.
fn token(text: &str) -> String {
    format!("b{}", STANDARD.encode(text))
}

/// A new workspace id: `w` and twelve hex digits.
fn new_workspace_id() -> String {
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

/// Workspaces on the sandbox host.
#[derive(Clone)]
pub struct SshProvider {
    runner: Arc<dyn Runner>,
    /// Held to write by a sweep and to read by an open, so a sweep never
    /// removes a workspace being made.
    gate: Arc<tokio::sync::RwLock<()>>,
}

impl std::fmt::Debug for SshProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshProvider").finish_non_exhaustive()
    }
}

impl SshProvider {
    /// A provider that reaches `target` over SSH.
    #[must_use]
    pub fn new(target: SshTarget) -> Self {
        Self::with_runner(Arc::new(SshRunner::new(target)))
    }

    pub(crate) fn with_runner(runner: Arc<dyn Runner>) -> Self {
        Self {
            runner,
            gate: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    /// Removes every workspace on the host: what a process that died left.
    /// One Henk per sandbox host, and only when it starts. The gate is taken
    /// at this call, not when the future first runs, so a workspace opened
    /// after it waits for the sweep; with workspaces being opened already,
    /// the sweep is refused.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the host cannot be reached or a
    /// workspace is being opened.
    pub fn sweep(&self) -> impl Future<Output = Result<(), WorkspaceError>> + Send + 'static {
        let gate = Arc::clone(&self.gate).try_write_owned();
        let runner = Arc::clone(&self.runner);
        async move {
            let _gate = gate.map_err(|_| {
                WorkspaceError::Backend(
                    "a workspace is being opened; the sandbox host is not swept".to_owned(),
                )
            })?;
            runner
                .call(&["sweep".to_owned()], &[], None, REQUEST_WAIT)
                .await?
                .ok("sweeping the sandbox host")
                .map(|_| ())
        }
    }

    /// What the runner says about itself and its tools.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] when the host cannot be reached or the
    /// runner does not answer.
    pub async fn probe(&self) -> Result<String, WorkspaceError> {
        let reply = self
            .runner
            .call(&["probe".to_owned()], &[], None, REQUEST_WAIT)
            .await?
            .ok("probing the sandbox host")?;
        Ok(String::from_utf8_lossy(&reply.stdout).into_owned())
    }
}

/// The checkout at `source` as a tar, `.git` included, links kept as links.
fn pack(source: &Path) -> Result<Vec<u8>, WorkspaceError> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    builder
        .append_dir_all(".", source)
        .and_then(|()| builder.into_inner())
        .map_err(|e| WorkspaceError::Backend(format!("cannot pack the checkout: {e}")))
}

#[async_trait::async_trait]
impl WorkspaceProvider for SshProvider {
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let source = source.to_owned();
        let archive = tokio::task::spawn_blocking(move || pack(&source))
            .await
            .map_err(|e| WorkspaceError::Backend(format!("cannot pack the checkout: {e}")))??;
        // A sweep in progress ends first.
        let _swept = self.gate.read().await;
        // The workspace exists before the host makes it, so any failure from
        // here on drops it, which removes whatever the host made: a `create`
        // that failed after its user was added, or a failed import.
        let mut workspace = SshWorkspace {
            runner: Arc::clone(&self.runner),
            id: new_workspace_id(),
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
                &archive,
                None,
                REQUEST_WAIT,
            )
            .await?
            .ok("importing the checkout")?;
        Ok(Arc::new(workspace))
    }
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

/// Lines matching `$3`, a [`Pattern`] read by PCRE (`grep -P`), in the
/// text files under `$1` of fewer than `$2` bytes, as `path NUL line:text`.
/// Files the run's user cannot read are left out first, so a grep that
/// says 2 failed on the pattern itself: no `-P`, a pattern PCRE refuses or
/// its backtracking limit. Each grep's 1, no match, becomes 0 and its 2
/// becomes 255, on which xargs stops and says 124, so that is an error and
/// never "no matches".
const SEARCH: &str = r#"find -P "$1" \( -iname .git -prune \) -o \( -type f -size -"$2"c -print0 \) |
xargs -0 -r sh -c 'p=$1
shift
for f; do
    shift
    if [ -r "$f" ]; then set -- "$@" "$f"; fi
done
[ "$#" -eq 0 ] || grep -HIPn --null -e "$p" -- "$@" || [ "$?" -eq 1 ] || exit 255' sh "$3""#;

/// `limit` in seconds for the runner's `timeout`, to the millisecond as the
/// host backend's limit is, rounded up: `timeout` reads 0 as no limit at all,
/// so what is left of a run is never sent as `0.000`.
fn seconds(limit: Duration) -> String {
    let millis = limit.as_nanos().div_ceil(1_000_000).max(1);
    format!("{}.{:03}", millis / 1000, millis % 1000)
}

/// A workspace on the sandbox host.
pub struct SshWorkspace {
    runner: Arc<dyn Runner>,
    id: String,
    /// The tree's absolute path on the host.
    work: String,
    budget: Budget,
    closed: AtomicBool,
}

impl std::fmt::Debug for SshWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshWorkspace")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SshWorkspace {
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
impl Workspace for SshWorkspace {
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
        let limit = self.budget.next(timeout)?;
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
        self.budget.spend(result.duration);
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
        max_file_bytes: u64,
        cap: usize,
    ) -> Result<Vec<Hit>, WorkspaceError> {
        let (_, real) = self.existing(dir).await?;
        let under = max_file_bytes.saturating_add(1).to_string();
        let reply = self
            .script(SEARCH, &[&real, &under, pattern.pcre()], &[])
            .await?
            .ok("searching")?;
        let mut hits = Vec::new();
        // `path NUL line:text NL`, repeated.
        let mut rest = reply.stdout.as_slice();
        while let Some(at) = rest.iter().position(|b| *b == 0) {
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
            let (Some(path), Some((number, text))) = (self.relative(file), line.split_once(':'))
            else {
                continue;
            };
            let Ok(number) = number.parse() else { continue };
            if only.is_some_and(|o| !o.matches(&path)) {
                continue;
            }
            hits.push(Hit {
                path,
                line: number,
                text: text.trim().to_owned(),
            });
        }
        hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        hits.truncate(cap);
        Ok(hits)
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
        let destroyed = self
            .runner
            .call(
                &["destroy".to_owned(), self.id.clone()],
                &[],
                None,
                REQUEST_WAIT,
            )
            .await
            .and_then(|reply| reply.ok("removing the workspace"));
        if let Err(error) = destroyed {
            warn!(%error, workspace = %self.id, "the sandbox host kept a workspace; the next sweep removes it");
        }
    }
}

impl SshWorkspace {
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

impl Drop for SshWorkspace {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // A run that was dropped instead of closed still leaves nothing: the
        // removal runs on its own, and the next sweep catches what it misses.
        let (runner, id) = (Arc::clone(&self.runner), self.id.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = runner
                    .call(&["destroy".to_owned(), id], &[], None, REQUEST_WAIT)
                    .await;
            });
        }
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

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use std::path::PathBuf;

    use crate::git::ScratchDir;

    /// The runner script of this repository, run here as the current user
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
        fn behind_a_link(name: &str) -> Arc<Self> {
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
            let script = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../deploy/sandbox/henk-runner"
            );
            // The request goes in as over SSH, so the runner's parse is tested.
            let mut child = tokio::process::Command::new("sh")
                .arg(script)
                .env_clear()
                .env("SSH_ORIGINAL_COMMAND", tokens.join(" "))
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HENK_RUNNER_BASE", &self.base)
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

    #[tokio::test]
    async fn the_ssh_backend_keeps_the_workspace_contract() {
        let runner = LocalRunner::new("henk-ssh-contract");
        let provider: Arc<dyn WorkspaceProvider> = Arc::new(SshProvider::with_runner(Arc::clone(
            &runner,
        )
            as Arc<dyn Runner>));
        crate::workspace::contract::every_backend_does_this(provider, "henk-ssh").await;
        let left: Vec<_> = std::fs::read_dir(runner.base()).unwrap().collect();
        assert!(left.is_empty(), "every closed workspace is gone: {left:?}");
    }

    #[tokio::test]
    async fn a_search_grep_cannot_finish_is_an_error_not_no_matches() {
        let runner = LocalRunner::new("henk-ssh-backtrack");
        let provider = SshProvider::with_runner(Arc::clone(&runner) as Arc<dyn Runner>);
        let source = crate::workspace::contract::source("henk-ssh-backtrack-src");
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        let line = format!("{}b\n", "a".repeat(5000));
        ws.write(&WorkspacePath::parse("long.txt").unwrap(), line.as_bytes())
            .await
            .unwrap();
        // Linear in Rust's regex, past PCRE's backtracking limit in grep -P.
        let pattern = Pattern::parse("(a+)+$").unwrap();
        let searched = ws
            .search(&WorkspacePath::root(), &pattern, None, 1 << 20, 10)
            .await;
        assert!(
            matches!(&searched, Err(WorkspaceError::Backend(m)) if m.contains("searching failed")),
            "{searched:?}"
        );
        ws.close().await;
    }

    #[tokio::test]
    async fn the_run_gets_its_own_checkout_with_git_and_a_dropped_one_is_removed() {
        let runner = LocalRunner::new("henk-ssh-own-checkout");
        let provider = SshProvider::with_runner(Arc::clone(&runner) as Arc<dyn Runner>);
        let source = crate::workspace::contract::source("henk-ssh-own-checkout-src");
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        let git = ws
            .exec(
                &["cat".to_owned(), ".git/config".to_owned()],
                &WorkspacePath::root(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(
            git.output, "[core]\n",
            "the checkout's own .git is there for commands"
        );
        drop(ws);
        for _ in 0..100 {
            if std::fs::read_dir(runner.base()).unwrap().next().is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("a dropped workspace is removed");
    }

    #[tokio::test]
    async fn a_tree_behind_a_linked_home_is_inside_the_repository() {
        let runner = LocalRunner::behind_a_link("henk-ssh-linked-home");
        let provider = SshProvider::with_runner(Arc::clone(&runner) as Arc<dyn Runner>);
        let source = crate::workspace::contract::source("henk-ssh-linked-home-src");
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        let read = ws
            .read(&WorkspacePath::parse("src/a.rs").unwrap(), 1024)
            .await
            .unwrap();
        assert_eq!(read, b"fn main() {\n    let x = 1;\n}\n");
        let ran = ws
            .exec(
                &["ls".to_owned()],
                &WorkspacePath::parse_dir("src").unwrap(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(ran.code, Some(0), "{}", ran.output);
        assert_eq!(ran.output, "a.rs\n");
        ws.close().await;
    }

    #[tokio::test]
    async fn a_tree_swapped_for_a_link_is_not_exported() {
        let runner = LocalRunner::new("henk-ssh-swapped-tree");
        let provider = SshProvider::with_runner(Arc::clone(&runner) as Arc<dyn Runner>);
        let source = crate::workspace::contract::source("henk-ssh-swapped-tree-src");
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        let ran = ws
            .exec(
                &[
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "cd .. && mkdir elsewhere && echo secret > elsewhere/secret && mv work kept && ln -s elsewhere work".to_owned(),
                ],
                &WorkspacePath::root(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(ran.code, Some(0), "{}", ran.output);
        let refused = ws.export().await.unwrap_err();
        assert!(
            refused.to_string().contains("not a directory any more"),
            "{refused}"
        );
        ws.close().await;
    }

    #[tokio::test]
    async fn sweep_removes_what_a_dead_process_left_and_probe_reports_the_runner() {
        let runner = LocalRunner::new("henk-ssh-sweep");
        let provider = SshProvider::with_runner(Arc::clone(&runner) as Arc<dyn Runner>);
        std::fs::create_dir_all(runner.base().join("w0123456789ab/home/work")).unwrap();
        std::fs::create_dir_all(runner.base().join("not-ours")).unwrap();
        provider.sweep().await.unwrap();
        let left: Vec<String> = std::fs::read_dir(runner.base())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["not-ours"]);
        let probe = provider.probe().await.unwrap();
        assert!(probe.starts_with("henk-runner 3\n"), "{probe}");
        assert!(probe.contains("\ngit "), "{probe}");
    }

    /// The local runner, with a `create` that fails once its user exists
    /// or a `sweep` that is slow to start.
    struct Twisted {
        inner: Arc<LocalRunner>,
        create_fails: bool,
        sweep_waits: Duration,
    }

    #[async_trait::async_trait]
    impl Runner for Twisted {
        async fn call(
            &self,
            tokens: &[String],
            stdin: &[u8],
            keep: Option<usize>,
            wait: Duration,
        ) -> Result<Reply, WorkspaceError> {
            let first = tokens.first().map(String::as_str);
            if first == Some("sweep") {
                tokio::time::sleep(self.sweep_waits).await;
            }
            let reply = self.inner.call(tokens, stdin, keep, wait).await?;
            if first == Some("create") && self.create_fails {
                return Ok(Reply {
                    code: Some(1),
                    stderr: b"useradd worked, then something did not".to_vec(),
                    ..Reply::default()
                });
            }
            Ok(reply)
        }
    }

    async fn emptied(base: &Path) -> bool {
        for _ in 0..100 {
            if std::fs::read_dir(base).unwrap().next().is_none() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_create_that_fails_after_its_user_exists_leaves_nothing() {
        let inner = LocalRunner::new("henk-ssh-create-fails");
        let runner = Arc::new(Twisted {
            inner: Arc::clone(&inner),
            create_fails: true,
            sweep_waits: Duration::ZERO,
        });
        let provider = SshProvider::with_runner(runner as Arc<dyn Runner>);
        let source = crate::workspace::contract::source("henk-ssh-create-fails-src");
        let Err(failed) = provider.open(source.path(), &Profile::default()).await else {
            panic!("the open failed");
        };
        assert!(
            failed.to_string().contains("making the workspace"),
            "{failed}"
        );
        assert!(
            emptied(inner.base()).await,
            "the half-made workspace is removed"
        );
    }

    #[tokio::test]
    async fn a_workspace_opened_while_the_sweep_runs_is_kept() {
        let inner = LocalRunner::new("henk-ssh-sweep-race");
        let runner = Arc::new(Twisted {
            inner: Arc::clone(&inner),
            create_fails: false,
            sweep_waits: Duration::from_millis(500),
        });
        let provider = SshProvider::with_runner(runner as Arc<dyn Runner>);
        // As `henk serve` does: the sweep starts, then runs come.
        let sweeping = tokio::spawn(provider.sweep());
        let source = crate::workspace::contract::source("henk-ssh-sweep-race-src");
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        sweeping.await.unwrap().unwrap();
        let read = ws
            .read(&WorkspacePath::parse("src/a.rs").unwrap(), 1024)
            .await
            .unwrap();
        assert_eq!(read, b"fn main() {\n    let x = 1;\n}\n");
        ws.close().await;
    }

    #[test]
    fn a_limit_is_sent_rounded_up_to_the_millisecond_and_never_as_zero() {
        assert_eq!(seconds(Duration::from_micros(3)), "0.001");
        assert_eq!(seconds(Duration::from_nanos(1)), "0.001");
        assert_eq!(seconds(Duration::from_millis(1500)), "1.500");
        assert_eq!(seconds(Duration::from_nanos(2_000_000_001)), "2.001");
        assert_eq!(seconds(Duration::from_mins(10)), "600.000");
    }

    #[tokio::test]
    async fn the_runner_refuses_what_is_not_a_request() {
        let runner = LocalRunner::new("henk-ssh-refuse");
        for tokens in [
            vec!["create".to_owned(), "../../etc".to_owned()],
            vec![
                "as".to_owned(),
                "w0123456789ab".to_owned(),
                "1;rm".to_owned(),
            ],
            vec![
                "record".to_owned(),
                "w0123456789ab".to_owned(),
                "blob".to_owned(),
                "HEAD".to_owned(),
            ],
            vec!["format-disk".to_owned()],
        ] {
            let reply = runner.call(&tokens, &[], None, REQUEST_WAIT).await.unwrap();
            assert_eq!(reply.code, Some(64), "{tokens:?}");
        }
    }

    /// The sandbox host of the live tests, from `HENK_TEST_SSH_HOST`,
    /// `_PORT`, `_USER`, `_KEY_PATH` and `_HOST_KEY` (an OpenSSH line).
    pub(crate) fn live_target() -> SshTarget {
        let var = |name: &str| {
            std::env::var(format!("HENK_TEST_SSH_{name}"))
                .unwrap_or_else(|_| panic!("HENK_TEST_SSH_{name} is not set"))
        };
        SshTarget {
            host: var("HOST"),
            port: var("PORT").parse().unwrap(),
            user: var("USER"),
            key: Arc::new(russh::keys::load_secret_key(var("KEY_PATH"), None).unwrap()),
            host_key: PublicKey::from_openssh(var("HOST_KEY").trim()).unwrap(),
        }
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host with henk-runner (HENK_TEST_SSH_*)"]
    async fn live_the_ssh_backend_keeps_the_workspace_contract_on_a_real_host() {
        let provider = SshProvider::new(live_target());
        let probe = provider.probe().await.unwrap();
        assert!(probe.starts_with("henk-runner "), "{probe}");
        assert!(!probe.contains("runuser missing"), "{probe}");
        // No sweep here: it removes every workspace on the host, including
        // those of live tests running alongside.
        crate::workspace::contract::every_backend_does_this(
            Arc::new(provider.clone()),
            "henk-ssh-live",
        )
        .await;
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host with henk-runner and mise (HENK_TEST_SSH_*)"]
    async fn live_mise_gives_the_run_the_repositorys_own_toolchain() {
        let provider = SshProvider::new(live_target());
        let source = ScratchDir::new("henk-ssh-live-mise").unwrap();
        std::fs::write(
            source.path().join("mise.toml"),
            "[tools]\nshellcheck = \"0.10.0\"\n",
        )
        .unwrap();
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        for step in [
            crate::workspace::toolchain::Mise::safe_mode(),
            crate::workspace::toolchain::Mise::install(),
        ] {
            let ran = ws
                .exec(&step, &WorkspacePath::root(), Duration::from_mins(5))
                .await
                .unwrap();
            assert_eq!(ran.code, Some(0), "{step:?}: {}", ran.output);
        }
        let mised = crate::workspace::toolchain::Mise::wrap(Arc::clone(&ws));
        let version = mised
            .exec(
                &["shellcheck".to_owned(), "--version".to_owned()],
                &WorkspacePath::root(),
                Duration::from_mins(1),
            )
            .await
            .unwrap();
        assert!(
            version.output.contains("version: 0.10.0"),
            "{}",
            version.output
        );
        assert!(
            ws.export().await.unwrap().is_empty(),
            "what mise fetched is outside the tree"
        );
        ws.close().await;
    }

    #[tokio::test]
    async fn a_host_that_accepts_and_says_nothing_is_given_up_on() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accepts, keeps the socket open and never speaks.
        let silent = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                held.push(socket);
            }
        });
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]),
        );
        let target = SshTarget {
            host: "127.0.0.1".to_owned(),
            port,
            user: "henk".to_owned(),
            key: Arc::new(key),
            host_key: PublicKey::from_openssh(&key_line(1)).unwrap(),
        };
        let runner = SshRunner {
            connect_wait: Duration::from_millis(300),
            ..SshRunner::new(target)
        };
        let provider = SshProvider::with_runner(Arc::new(runner) as Arc<dyn Runner>);
        let given_up = tokio::time::timeout(Duration::from_secs(10), provider.probe())
            .await
            .expect("the probe ends on its own")
            .unwrap_err();
        assert!(
            given_up.to_string().contains("did not answer"),
            "{given_up}"
        );
        silent.abort();
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host with henk-runner (HENK_TEST_SSH_*)"]
    async fn live_a_wrong_host_key_is_refused() {
        let target = SshTarget {
            host_key: PublicKey::from_openssh(&key_line(9)).unwrap(),
            ..live_target()
        };
        let refused = SshProvider::new(target).probe().await.unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("not the pinned workspace.ssh.host_key"),
            "{refused}"
        );
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host with henk-runner (HENK_TEST_SSH_*)"]
    async fn live_export_stops_the_run_and_refuses_a_tree_swapped_for_a_link() {
        let provider = SshProvider::new(live_target());
        let source = crate::workspace::contract::source("henk-ssh-live-swap-src");
        let ws = provider
            .open(source.path(), &Profile::default())
            .await
            .unwrap();
        let sh = |script: &str| vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()];
        let left = ws
            .exec(
                &sh("(sleep 300 >/dev/null 2>&1 &); ln -s /root rootlink"),
                &WorkspacePath::root(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(left.code, Some(0), "{}", left.output);
        let changes = ws.export().await.unwrap();
        assert_eq!(changes.len(), 1, "only the link itself is a change");
        assert_eq!(
            changes[0].raw.mode,
            henk_domain::workspace::FileMode::Symlink
        );
        let after = ws
            .exec(
                &sh("pgrep -x sleep"),
                &WorkspacePath::root(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(
            after.code,
            Some(1),
            "export stopped what the run left behind: {}",
            after.output
        );
        let swapped = ws
            .exec(
                &sh("cd .. && mv work kept && ln -s /etc work"),
                &WorkspacePath::root(),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(swapped.code, Some(0), "{}", swapped.output);
        let refused = ws.export().await.unwrap_err();
        assert!(
            refused.to_string().contains("not a directory any more"),
            "{refused}"
        );
        ws.close().await;
    }

    fn key_line(seed: u8) -> String {
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]),
        );
        key.public_key().to_openssh().unwrap()
    }

    #[test]
    fn only_the_pinned_host_key_is_accepted() {
        let pinned = PublicKey::from_openssh(&key_line(1)).unwrap();
        let same = russh::keys::PublicKeyOrCertificate::PublicKey {
            key: PublicKey::from_openssh(&key_line(1)).unwrap(),
            hash_alg: None,
        };
        let other = russh::keys::PublicKeyOrCertificate::PublicKey {
            key: PublicKey::from_openssh(&key_line(2)).unwrap(),
            hash_alg: None,
        };
        assert!(host_key_matches(&pinned, &same));
        assert!(
            !host_key_matches(&pinned, &other),
            "a different host key is refused"
        );
    }

    #[test]
    fn a_value_token_is_never_empty() {
        assert_eq!(token(""), "b");
        assert_eq!(token("sh"), "bc2g=");
        assert!(
            token("a b\n")
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c))
        );
    }

    #[test]
    fn a_kept_stream_holds_its_end() {
        let mut kept = Kept::new(Some(4));
        let long = vec![b'x'; 200_000];
        for chunk in [b"abc".as_slice(), b"defgh", &long, b"tail"] {
            kept.push(chunk).unwrap();
        }
        assert_eq!(kept.finish(), b"tail");
        let mut whole = Kept::new(None);
        whole.push(b"all of it").unwrap();
        assert_eq!(whole.finish(), b"all of it");
    }

    #[test]
    fn workspace_ids_are_w_and_twelve_hex_digits() {
        let (a, b) = (new_workspace_id(), new_workspace_id());
        for id in [&a, &b] {
            assert_eq!(id.len(), 13);
            assert!(id.starts_with('w') && id[1..].bytes().all(|c| c.is_ascii_hexdigit()));
        }
        assert_ne!(a, b);
    }
}
