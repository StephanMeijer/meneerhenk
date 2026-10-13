//! The ssh backend (#84): a workspace on a sandbox host, reached over SSH.
//!
//! Each workspace is a throwaway user on that host with its own checkout of
//! the pull request, `.git` and all, in `~/work`. Henk signs in as root and
//! sends its own script with every request (`sandbox.sh`; nothing of Henk's
//! is installed there), which makes the user, runs commands as it, keeps a
//! record of the checkout that only root can write, and removes the user
//! again. The model never runs as root: only Henk's fixed requests do. Every file and command the
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use henk_domain::workspace::Profile;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{ChannelMsg, client};

use super::remote::{
    Kept, REQUEST_WAIT, RemoteWorkspace, Reply, Runner, command_line, new_workspace_id, pack,
};
use super::{Workspace, WorkspaceError, WorkspaceProvider};

/// How long making the connection and signing in may take. The connection
/// is shared, so a host that accepts and then says nothing must not hold
/// every other request up for longer than this.
const CONNECT_WAIT: Duration = Duration::from_secs(30);

/// Where the sandbox host is and how Henk proves who he is and checks who
/// it is.
#[derive(Clone)]
pub struct SshTarget {
    /// Host name or address.
    pub host: String,
    /// SSH port.
    pub port: u16,
    /// The user Henk signs in as: root, or one with passwordless sudo.
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
                WorkspaceError::Unreachable(format!(
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
            WorkspaceError::Unreachable(format!(
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
            russh::Error::UnknownKey => WorkspaceError::Unreachable(format!(
                "{}:{} offered a host key that is not the pinned host_key",
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
            return Err(WorkspaceError::Unreachable(format!(
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
    fn location(&self) -> Option<String> {
        Some(format!(
            "{}@{}:{}",
            self.target.user, self.target.host, self.target.port
        ))
    }

    async fn call(
        &self,
        tokens: &[String],
        stdin: &[u8],
        keep: Option<usize>,
        wait: Duration,
    ) -> Result<Reply, WorkspaceError> {
        let handle = self.connect().await?;
        let lost = |error: russh::Error| {
            WorkspaceError::Unreachable(format!(
                "the connection to the sandbox host failed: {error}"
            ))
        };
        let exchange = async {
            let mut channel = handle.channel_open_session().await.map_err(lost)?;
            channel
                .exec(true, command_line(&self.target.user, tokens)?)
                .await
                .map_err(lost)?;
            if !stdin.is_empty() {
                channel.data(stdin).await.map_err(lost)?;
            }
            channel.eof().await.map_err(lost)?;
            let (mut out, mut err) = (Kept::new(keep), Kept::new(keep));
            let mut code = None;
            let mut detail = None;
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Data { data } => out.push(&data)?,
                    ChannelMsg::ExtendedData { data, ext: 1 } => err.push(&data)?,
                    ChannelMsg::ExitStatus { exit_status } => {
                        code = i32::try_from(exit_status).ok();
                    }
                    ChannelMsg::ExitSignal {
                        signal_name,
                        core_dumped,
                        error_message,
                        ..
                    } => {
                        let name = match signal_name {
                            russh::Sig::Custom(name) => name,
                            other => format!("{other:?}"),
                        };
                        detail = Some(signal_detail(&name, core_dumped, &error_message));
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
                detail,
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

/// What a request killed by a signal says about it (#339).
fn signal_detail(name: &str, core_dumped: bool, message: &str) -> String {
    let mut said = format!("killed by signal {name}");
    if core_dumped {
        said.push_str(", core dumped");
    }
    let message = message.trim();
    if !message.is_empty() {
        said.push_str(": ");
        said.push_str(message);
    }
    said
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

#[async_trait::async_trait]
impl WorkspaceProvider for SshProvider {
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        let archive = packed(source).await?;
        self.open_packed(&archive, profile).await
    }
}

/// The checkout at `source` as an archive to import.
async fn packed(source: &Path) -> Result<Vec<u8>, WorkspaceError> {
    let source = source.to_owned();
    tokio::task::spawn_blocking(move || pack(&source))
        .await
        .map_err(|e| WorkspaceError::Backend(format!("cannot pack the checkout: {e}")))?
}

impl SshProvider {
    /// A workspace holding `archive`, made on this host.
    async fn open_packed(
        &self,
        archive: &[u8],
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        // A sweep in progress ends first.
        let _swept = self.gate.read().await;
        let workspace = RemoteWorkspace::start(
            Arc::clone(&self.runner),
            new_workspace_id(),
            profile,
            archive,
        )
        .await?;
        Ok(Arc::new(workspace))
    }
}

/// How long a host that could not be reached is tried only after the
/// others (#389).
const DOWN_FOR: Duration = Duration::from_mins(1);

/// A host's runner that counts its workspaces: one more when a workspace is
/// started on it, one fewer when that workspace is released, which a
/// workspace does exactly once, closed, dropped or failed half-way.
struct CountingRunner {
    id: String,
    inner: Arc<dyn Runner>,
    live: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Runner for CountingRunner {
    async fn call(
        &self,
        tokens: &[String],
        stdin: &[u8],
        keep: Option<usize>,
        wait: Duration,
    ) -> Result<Reply, WorkspaceError> {
        self.inner.call(tokens, stdin, keep, wait).await
    }

    fn location(&self) -> Option<String> {
        Some(match self.inner.location() {
            Some(at) => format!("ssh {} ({at})", self.id),
            None => format!("ssh {}", self.id),
        })
    }

    async fn release(&self, id: &str) -> Result<(), WorkspaceError> {
        let released = self.inner.release(id).await;
        let _ = self
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        released
    }
}

/// One host of the pool.
struct PooledHost {
    id: String,
    provider: SshProvider,
    live: Arc<AtomicUsize>,
    /// Until when the host counts as down, after it could not be reached.
    down_until: std::sync::Mutex<Option<Instant>>,
}

impl PooledHost {
    fn is_down(&self, now: Instant) -> bool {
        self.down_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| until > now)
    }

    fn mark_down(&self) {
        if let Ok(mut until) = self.down_until.lock() {
            *until = Some(Instant::now() + DOWN_FOR);
        }
    }
}

/// The sandbox hosts of the ssh backend (#389). A workspace opens on the
/// least busy host its profile may use; a host that cannot be reached is
/// passed over, and tried only after the others for a minute.
pub struct SshPool {
    hosts: Vec<PooledHost>,
    /// Held while a host is chosen and counted, so that opens which start
    /// together do not all read the same counts and pick the same host.
    choosing: std::sync::Mutex<()>,
}

impl std::fmt::Debug for SshPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.hosts.iter().map(|h| &h.id))
            .finish()
    }
}

impl SshPool {
    /// A pool of the hosts `targets` names, by id.
    #[must_use]
    pub fn new(targets: Vec<(String, SshTarget)>) -> Self {
        Self::with_runners(
            targets
                .into_iter()
                .map(|(id, target)| (id, Arc::new(SshRunner::new(target)) as Arc<dyn Runner>))
                .collect(),
        )
    }

    pub(crate) fn with_runners(runners: Vec<(String, Arc<dyn Runner>)>) -> Self {
        let hosts = runners
            .into_iter()
            .map(|(id, inner)| {
                let live = Arc::new(AtomicUsize::new(0));
                let counting = CountingRunner {
                    id: id.clone(),
                    inner,
                    live: Arc::clone(&live),
                };
                PooledHost {
                    id,
                    provider: SshProvider::with_runner(Arc::new(counting)),
                    live,
                    down_until: std::sync::Mutex::new(None),
                }
            })
            .collect();
        Self {
            hosts,
            choosing: std::sync::Mutex::new(()),
        }
    }

    /// Sweeps every host at once (see [`SshProvider::sweep`]); one host
    /// that fails does not stop the others.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError`] naming every host that was not swept.
    pub fn sweep(&self) -> impl Future<Output = Result<(), WorkspaceError>> + Send + 'static {
        let sweeps: Vec<_> = self
            .hosts
            .iter()
            .map(|h| (h.id.clone(), h.provider.sweep()))
            .collect();
        async move {
            let mut running = tokio::task::JoinSet::new();
            for (id, sweep) in sweeps {
                running.spawn(async move { (id, sweep.await) });
            }
            let mut failed = Vec::new();
            while let Some(done) = running.join_next().await {
                match done {
                    Ok((id, Ok(()))) => tracing::info!(host = %id, "sandbox host swept"),
                    Ok((id, Err(error))) => {
                        tracing::warn!(host = %id, %error, "could not sweep the sandbox host");
                        failed.push(format!("{id}: {error}"));
                    }
                    Err(error) => failed.push(format!("a sweep stopped: {error}")),
                }
            }
            if failed.is_empty() {
                Ok(())
            } else {
                failed.sort();
                Err(WorkspaceError::Backend(format!(
                    "not every sandbox host was swept: {}",
                    failed.join("; ")
                )))
            }
        }
    }

    /// The hosts `profile` may use.
    fn allowed<'h>(&'h self, profile: &Profile) -> impl Iterator<Item = &'h PooledHost> {
        self.hosts
            .iter()
            .filter(|h| profile.hosts.is_empty() || profile.hosts.contains(&h.id))
    }

    /// The host to try next, counted for the workspace about to start on
    /// it: up before down, then the fewest workspaces, then by id. A host
    /// in `tried` is not chosen again. Choosing and counting happen under
    /// one lock, so opens that run together see each other.
    fn reserve<'h>(&'h self, profile: &Profile, tried: &[&str]) -> Option<&'h PooledHost> {
        let _choosing = self
            .choosing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let host = self
            .allowed(profile)
            .filter(|h| !tried.contains(&h.id.as_str()))
            .min_by_key(|h| (h.is_down(now), h.live.load(Ordering::Acquire), h.id.clone()))?;
        // Counted before it is started: a start that fails releases the
        // half-made workspace, which counts it off again.
        host.live.fetch_add(1, Ordering::AcqRel);
        Some(host)
    }
}

#[async_trait::async_trait]
impl WorkspaceProvider for SshPool {
    async fn open(
        &self,
        source: &Path,
        profile: &Profile,
    ) -> Result<Arc<dyn Workspace>, WorkspaceError> {
        if self.allowed(profile).next().is_none() {
            return Err(WorkspaceError::Backend(format!(
                "no sandbox host is one of the profile's hosts ({})",
                profile.hosts.join(", ")
            )));
        }
        let archive = packed(source).await?;
        let mut refused = Vec::new();
        let mut tried = Vec::new();
        // The host is chosen only now, after the await, from the counts as
        // they are, and counted in the same step.
        while let Some(host) = self.reserve(profile, &tried) {
            tried.push(host.id.as_str());
            match host.provider.open_packed(&archive, profile).await {
                Ok(workspace) => return Ok(workspace),
                Err(WorkspaceError::Unreachable(why)) => {
                    tracing::warn!(host = %host.id, %why, "sandbox host passed over");
                    host.mark_down();
                    refused.push(format!("{}: {why}", host.id));
                }
                Err(other) => return Err(other),
            }
        }
        Err(WorkspaceError::Unreachable(format!(
            "no sandbox host could be used: {}",
            refused.join("; ")
        )))
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

    use super::super::remote::tests::LocalRunner;
    use super::super::remote::*;
    use super::*;
    use crate::git::ScratchDir;
    use crate::workspace::Pattern;
    use henk_domain::path::WorkspacePath;

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
            .search(&WorkspacePath::root(), &pattern, None, 0, 1 << 20, 10)
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
        assert!(probe.starts_with("henk-sandbox 5\n"), "{probe}");
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
        let unsafe_token = [
            "as".to_owned(),
            "w0123456789ab".to_owned(),
            "1;rm".to_owned(),
        ];
        assert!(
            runner
                .call(&unsafe_token, &[], None, REQUEST_WAIT)
                .await
                .is_err(),
            "Henk refuses it before anything is sent"
        );
        // And the script refuses it too, should it ever arrive.
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(SCRIPT)
            .arg("henk-sandbox")
            .args(&unsafe_token)
            .env("HENK_SANDBOX_BASE", runner.base())
            .output()
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(64));
        assert!(String::from_utf8_lossy(&out.stderr).contains("unexpected characters"));
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
    #[ignore = "needs a sandbox host (HENK_TEST_SSH_*)"]
    async fn live_the_ssh_backend_keeps_the_workspace_contract_on_a_real_host() {
        let provider = SshProvider::new(live_target());
        let probe = provider.probe().await.unwrap();
        assert!(probe.starts_with("henk-sandbox "), "{probe}");
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
    #[ignore = "needs a sandbox host with mise (HENK_TEST_SSH_*)"]
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
            matches!(&given_up, WorkspaceError::Unreachable(why) if why.contains("did not answer")),
            "{given_up}"
        );
        silent.abort();
    }

    #[tokio::test]
    async fn a_host_that_refuses_the_connection_is_unreachable() {
        // A port that was free a moment ago: nothing listens there.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]),
        );
        let provider = SshProvider::new(SshTarget {
            host: "127.0.0.1".to_owned(),
            port,
            user: "henk".to_owned(),
            key: Arc::new(key),
            host_key: PublicKey::from_openssh(&key_line(1)).unwrap(),
        });
        let refused = provider.probe().await.unwrap_err();
        assert!(
            matches!(refused, WorkspaceError::Unreachable(_)),
            "{refused}"
        );
    }

    /// A host that cannot be reached: every request fails as such. Counts
    /// the workspaces asked of it.
    struct Down {
        creates: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Runner for Down {
        async fn call(
            &self,
            tokens: &[String],
            _: &[u8],
            _: Option<usize>,
            _: Duration,
        ) -> Result<Reply, WorkspaceError> {
            if tokens.first().map(String::as_str) == Some("create") {
                self.creates.fetch_add(1, Ordering::SeqCst);
            }
            Err(WorkspaceError::Unreachable(
                "the sandbox host 10.0.0.9:22 did not answer within 30s".to_owned(),
            ))
        }
    }

    fn down() -> Arc<Down> {
        Arc::new(Down {
            creates: AtomicUsize::new(0),
        })
    }

    fn pool(hosts: Vec<(&str, Arc<dyn Runner>)>) -> SshPool {
        SshPool::with_runners(
            hosts
                .into_iter()
                .map(|(id, runner)| (id.to_owned(), runner))
                .collect(),
        )
    }

    fn workspaces(runner: &LocalRunner) -> usize {
        std::fs::read_dir(runner.base()).unwrap().count()
    }

    #[tokio::test]
    async fn workspaces_spread_over_the_hosts_and_each_says_where_it_runs() {
        let (a, b) = (
            LocalRunner::new("henk-pool-a"),
            LocalRunner::new("henk-pool-b"),
        );
        let pool = pool(vec![
            ("a", Arc::clone(&a) as Arc<dyn Runner>),
            ("b", Arc::clone(&b) as Arc<dyn Runner>),
        ]);
        let source = crate::workspace::contract::source("henk-pool-spread-src");
        let first = pool.open(source.path(), &Profile::default()).await.unwrap();
        let second = pool.open(source.path(), &Profile::default()).await.unwrap();
        assert_eq!((workspaces(&a), workspaces(&b)), (1, 1), "one on each");
        assert_eq!(first.location().as_deref(), Some("ssh a"));
        assert_eq!(second.location().as_deref(), Some("ssh b"));
        first.close().await;
        second.close().await;
        assert_eq!((workspaces(&a), workspaces(&b)), (0, 0));
        // Both idle again: the next goes to the first by id.
        let third = pool.open(source.path(), &Profile::default()).await.unwrap();
        assert_eq!(third.location().as_deref(), Some("ssh a"));
        third.close().await;
    }

    #[tokio::test]
    async fn workspaces_opened_at_once_still_spread_over_the_hosts() {
        let (a, b) = (
            LocalRunner::new("henk-pool-burst-a"),
            LocalRunner::new("henk-pool-burst-b"),
        );
        let pool = Arc::new(pool(vec![
            ("a", Arc::clone(&a) as Arc<dyn Runner>),
            ("b", Arc::clone(&b) as Arc<dyn Runner>),
        ]));
        let source = crate::workspace::contract::source("henk-pool-burst-src");
        let mut opening = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let (pool, path) = (Arc::clone(&pool), source.path().to_owned());
            opening.spawn(async move { pool.open(&path, &Profile::default()).await });
        }
        let mut open = Vec::new();
        while let Some(done) = opening.join_next().await {
            open.push(done.unwrap().unwrap());
        }
        assert_eq!(
            (workspaces(&a), workspaces(&b)),
            (3, 3),
            "a burst is spread, not sent to one host"
        );
        for workspace in open {
            workspace.close().await;
        }
    }

    #[tokio::test]
    async fn a_host_that_cannot_be_reached_is_passed_over_and_then_tried_last() {
        let (gone, b) = (down(), LocalRunner::new("henk-pool-up"));
        let pool = pool(vec![
            ("a", Arc::clone(&gone) as Arc<dyn Runner>),
            ("b", Arc::clone(&b) as Arc<dyn Runner>),
        ]);
        let source = crate::workspace::contract::source("henk-pool-down-src");
        let first = pool.open(source.path(), &Profile::default()).await.unwrap();
        assert_eq!(first.location().as_deref(), Some("ssh b"));
        assert_eq!(gone.creates.load(Ordering::SeqCst), 1, "a was tried first");
        first.close().await;
        let second = pool.open(source.path(), &Profile::default()).await.unwrap();
        assert_eq!(second.location().as_deref(), Some("ssh b"));
        assert_eq!(
            gone.creates.load(Ordering::SeqCst),
            1,
            "a is down for a while: not tried while b works"
        );
        second.close().await;
    }

    #[tokio::test]
    async fn with_no_host_reachable_the_error_names_each() {
        let pool = pool(vec![
            ("a", down() as Arc<dyn Runner>),
            ("b", down() as Arc<dyn Runner>),
        ]);
        let source = crate::workspace::contract::source("henk-pool-none-src");
        let Err(error) = pool.open(source.path(), &Profile::default()).await else {
            panic!("no host can open it");
        };
        let text = error.to_string();
        assert!(
            text.contains("no sandbox host could be used")
                && text.contains("a: the sandbox host")
                && text.contains("b: the sandbox host"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_failure_on_a_working_host_is_not_tried_on_another() {
        let inner = LocalRunner::new("henk-pool-fails");
        let b = LocalRunner::new("henk-pool-spare");
        let pool = pool(vec![
            (
                "a",
                Arc::new(Twisted {
                    inner: Arc::clone(&inner),
                    create_fails: true,
                    sweep_waits: Duration::ZERO,
                }) as Arc<dyn Runner>,
            ),
            ("b", Arc::clone(&b) as Arc<dyn Runner>),
        ]);
        let source = crate::workspace::contract::source("henk-pool-fails-src");
        let Err(error) = pool.open(source.path(), &Profile::default()).await else {
            panic!("the open failed");
        };
        assert!(
            error.to_string().contains("making the workspace"),
            "{error}"
        );
        assert_eq!(workspaces(&b), 0, "b was not asked");
    }

    #[tokio::test]
    async fn a_profile_opens_only_on_the_hosts_it_names() {
        let (a, b) = (
            LocalRunner::new("henk-pool-named-a"),
            LocalRunner::new("henk-pool-named-b"),
        );
        let pool = pool(vec![
            ("a", Arc::clone(&a) as Arc<dyn Runner>),
            ("b", Arc::clone(&b) as Arc<dyn Runner>),
        ]);
        let source = crate::workspace::contract::source("henk-pool-named-src");
        let profile = Profile {
            hosts: vec!["b".to_owned()],
            ..Profile::default()
        };
        let ws = pool.open(source.path(), &profile).await.unwrap();
        assert_eq!((workspaces(&a), workspaces(&b)), (0, 1));
        ws.close().await;
    }

    #[tokio::test]
    async fn every_host_is_swept_and_one_that_fails_does_not_stop_the_rest() {
        let a = LocalRunner::new("henk-pool-sweep-a");
        std::fs::create_dir_all(a.base().join("w0123456789ab/home/work")).unwrap();
        let pool = pool(vec![
            ("a", Arc::clone(&a) as Arc<dyn Runner>),
            ("b", down() as Arc<dyn Runner>),
        ]);
        let error = pool.sweep().await.unwrap_err().to_string();
        assert!(error.contains("b: "), "{error}");
        assert!(!error.contains("a: "), "{error}");
        assert_eq!(workspaces(&a), 0, "a was swept");
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host (HENK_TEST_SSH_*)"]
    async fn live_a_wrong_host_key_is_refused() {
        let target = SshTarget {
            host_key: PublicKey::from_openssh(&key_line(9)).unwrap(),
            ..live_target()
        };
        let refused = SshProvider::new(target).probe().await.unwrap_err();
        assert!(
            refused.to_string().contains("not the pinned host_key"),
            "{refused}"
        );
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host (HENK_TEST_SSH_*)"]
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
    fn a_request_is_the_quoted_script_and_safe_tokens() {
        let tokens = [
            "probe".to_owned(),
            token("it's"),
            "w0123456789ab".to_owned(),
        ];
        let line = command_line("root", &tokens).unwrap();
        assert!(line.starts_with("sh -c '#!/bin/sh\n"), "{line}");
        assert!(line.ends_with(&format!(
            " henk-sandbox probe {} w0123456789ab",
            token("it's")
        )));
        assert!(
            command_line("deploy", &tokens)
                .unwrap()
                .starts_with("sudo -n sh -c '")
        );
        for bad in ["a b", "x;rm", "$(id)", "'", ""] {
            let tokens = ["probe".to_owned(), bad.to_owned()];
            assert!(command_line("root", &tokens).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn the_shell_hands_the_script_exactly_its_tokens() {
        // The script, quoted as it is sent, run by a local shell: its own
        // single quotes survive, and the tokens arrive as they were.
        let tokens = ["nothing".to_owned(), token("a'b"), token("")];
        let line = command_line("root", &tokens).unwrap();
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(line.replacen("sh -c '", "sh -c 'printf \"%s\\n\" \"$@\"; exit 0;", 1))
            .output()
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("nothing\n{}\n{}\n", token("a'b"), token(""))
        );
        assert!(
            SCRIPT.contains('\''),
            "the test means something only if the script has quotes"
        );
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

    #[test]
    fn a_request_killed_by_a_signal_says_so() {
        assert_eq!(signal_detail("KILL", false, ""), "killed by signal KILL");
        assert_eq!(
            signal_detail("SEGV", true, " out of bounds \n"),
            "killed by signal SEGV, core dumped: out of bounds"
        );
    }
}
