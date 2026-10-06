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
use std::time::Duration;

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
        let source = source.to_owned();
        let archive = tokio::task::spawn_blocking(move || pack(&source))
            .await
            .map_err(|e| WorkspaceError::Backend(format!("cannot pack the checkout: {e}")))??;
        // A sweep in progress ends first.
        let _swept = self.gate.read().await;
        let workspace = RemoteWorkspace::start(
            Arc::clone(&self.runner),
            new_workspace_id(),
            profile,
            &archive,
        )
        .await?;
        Ok(Arc::new(workspace))
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
    use henk_domain::address::WorkspacePath;

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
            given_up.to_string().contains("did not answer"),
            "{given_up}"
        );
        silent.abort();
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
            refused
                .to_string()
                .contains("not the pinned workspace.ssh.host_key"),
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
}
