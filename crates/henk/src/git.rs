//! A throwaway checkout of one pull request's branch, for an address run
//! (§3.5): clone, see what changed, commit, push. Git runs as a child
//! process with an empty environment. The credential reaches git through its
//! environment, never its arguments, so it is not in a process listing, a
//! log line or anything a model sees.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use henk_domain::review::CommitSha;
use henk_domain::workspace::{Change, ChangeKind};
use henk_platform::address::{CommitIdentity, GitCredential};
use secrecy::ExposeSecret as _;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

/// How long one git command may take.
const GIT_TIMEOUT: Duration = Duration::from_mins(5);

/// How long `git --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether `git` runs in Henk's own environment (#254): its version line,
/// or why it could not run. `path` replaces `PATH` when given, for a test.
///
/// # Errors
///
/// Returns why git did not run or answer, in words.
pub async fn version(path: Option<&std::ffi::OsStr>) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("--version")
        .env_clear()
        .env(
            "PATH",
            path.map_or_else(
                || std::env::var_os("PATH").unwrap_or_default(),
                ToOwned::to_owned,
            ),
        )
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(VERSION_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("git could not run: {error}")),
        Err(_) => return Err("git --version did not answer".to_owned()),
    };
    if !output.status.success() {
        return Err(format!("git --version failed ({})", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Why a git step failed.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Git could not be started or waited for.
    #[error("git could not run: {0}")]
    Spawn(#[from] std::io::Error),
    /// Git ran past its time limit.
    #[error("git {0} took longer than {GIT_TIMEOUT:?}")]
    Timeout(String),
    /// Git said no.
    #[error("git {command} failed: {stderr}")]
    Failed {
        /// The subcommand.
        command: String,
        /// What git printed, without credentials.
        stderr: String,
    },
    /// The branch is no longer at the commit Henk read.
    #[error("the branch moved: expected {expected}, found {found}")]
    HeadMoved {
        /// What Henk read.
        expected: String,
        /// What is there.
        found: String,
    },
    /// The push was refused as not a fast-forward.
    #[error("the push was refused: the branch moved since Henk read it")]
    Rejected,
    /// A change could not be applied to the checkout.
    #[error("cannot apply {path}: {why}")]
    Apply {
        /// The path in the checkout.
        path: String,
        /// Why not.
        why: String,
    },
}

/// A directory that is removed when dropped.
#[derive(Debug)]
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    /// Makes `name` under the system temp directory, empty.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the directory cannot be made.
    pub fn new(name: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One checkout of a branch.
#[derive(Debug)]
pub struct Checkout {
    dir: ScratchDir,
    credential: Option<GitCredential>,
}

/// The header git sends to authenticate: Basic with the platform's user name
/// for tokens (`x-access-token` on GitHub, `oauth2` on GitLab).
fn auth_header(credential: &GitCredential) -> String {
    let basic = STANDARD.encode(format!(
        "{}:{}",
        credential.username,
        credential.token.expose_secret()
    ));
    format!("Authorization: Basic {basic}")
}

/// A git command with nothing inherited: no user or system config, no
/// prompt, and the credential only when one is given.
pub(crate) fn git_command(
    dir: &Path,
    args: &[&str],
    credential: Option<&GitCredential>,
) -> Command {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("LANG", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(credential) = credential {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", auth_header(credential));
    }
    command
}

/// Runs git and returns its stdout as is. `input` goes to stdin.
async fn run(
    dir: &Path,
    args: &[&str],
    credential: Option<&GitCredential>,
    input: Option<&str>,
) -> Result<String, GitError> {
    let name = args.first().copied().unwrap_or("").to_owned();
    let mut child = git_command(dir, args, credential).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(input) = input {
            stdin.write_all(input.as_bytes()).await?;
        }
        drop(stdin);
    }
    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| GitError::Timeout(name.clone()))??;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if name == "push"
        && (stderr.contains("[rejected]")
            || stderr.contains("non-fast-forward")
            || stderr.contains("fetch first"))
    {
        return Err(GitError::Rejected);
    }
    Err(GitError::Failed {
        command: name,
        stderr,
    })
}

impl Checkout {
    /// Clones `branch` of `remote` into `dir` and checks that its head is
    /// `expected`: a branch that moved since Henk read it is not worked on.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when the clone fails or the head moved.
    pub async fn clone_at(
        dir: ScratchDir,
        remote: &str,
        branch: &str,
        expected: &CommitSha,
        credential: Option<GitCredential>,
    ) -> Result<Self, GitError> {
        let checkout = Self { dir, credential };
        run(
            checkout.path(),
            &[
                "clone",
                "--quiet",
                "--depth",
                "50",
                "--single-branch",
                "--branch",
                branch,
                remote,
                ".",
            ],
            checkout.credential.as_ref(),
            None,
        )
        .await?;
        let found = checkout.head().await?;
        if !found.eq_ignore_ascii_case(expected.as_str()) {
            return Err(GitError::HeadMoved {
                expected: expected.as_str().to_owned(),
                found,
            });
        }
        Ok(checkout)
    }

    /// Fetches exactly `commit` of `remote` into `dir` and checks it out,
    /// detached: a review's workspace holds the commit it reviews, even when
    /// the branch moved on since (#170). Only that commit is fetched.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when the remote does not serve the commit.
    pub async fn fetch_at(
        dir: ScratchDir,
        remote: &str,
        commit: &CommitSha,
        credential: Option<GitCredential>,
    ) -> Result<Self, GitError> {
        let checkout = Self { dir, credential };
        run(checkout.path(), &["init", "--quiet"], None, None).await?;
        run(
            checkout.path(),
            &[
                "fetch",
                "--quiet",
                "--depth",
                "1",
                "--no-tags",
                remote,
                commit.as_str(),
            ],
            checkout.credential.as_ref(),
            None,
        )
        .await?;
        run(
            checkout.path(),
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            None,
            None,
        )
        .await?;
        let found = checkout.head().await?;
        if !found.eq_ignore_ascii_case(commit.as_str()) {
            return Err(GitError::HeadMoved {
                expected: commit.as_str().to_owned(),
                found,
            });
        }
        Ok(checkout)
    }

    /// The working tree.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The commit checked out.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when git cannot say.
    pub async fn head(&self) -> Result<String, GitError> {
        Ok(run(self.path(), &["rev-parse", "HEAD"], None, None)
            .await?
            .trim()
            .to_owned())
    }

    /// Applies a checked changeset (§3.5): writes or removes each file and
    /// sets its executable bit. A path under or at a symbolic link of the
    /// checkout is refused, so no change lands outside it.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::Apply`] for the first change that cannot be
    /// applied; the checkout is then thrown away, nothing is pushed.
    pub fn apply(&self, changes: &[Change]) -> Result<(), GitError> {
        use std::os::unix::fs::PermissionsExt as _;
        for change in changes {
            let shown = change.path.as_str();
            let fail = |why: String| GitError::Apply {
                path: shown.to_owned(),
                why,
            };
            let target = self.path().join(shown);
            let mut walked = self.path().to_path_buf();
            for part in shown.split('/') {
                walked.push(part);
                if walked
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink())
                {
                    return Err(fail("a symbolic link is in the way".to_owned()));
                }
            }
            match &change.kind {
                ChangeKind::Delete => match std::fs::remove_file(&target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(fail(error.to_string())),
                },
                ChangeKind::Write {
                    content,
                    executable,
                } => {
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| fail(e.to_string()))?;
                    }
                    std::fs::write(&target, content).map_err(|e| fail(e.to_string()))?;
                    let mut permissions = std::fs::metadata(&target)
                        .map_err(|e| fail(e.to_string()))?
                        .permissions();
                    let mode = permissions.mode();
                    permissions.set_mode(if *executable {
                        mode | 0o111
                    } else {
                        mode & !0o111
                    });
                    std::fs::set_permissions(&target, permissions)
                        .map_err(|e| fail(e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    /// The paths changed in the working tree, added and deleted ones too.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when git cannot say.
    pub async fn changed_files(&self) -> Result<Vec<String>, GitError> {
        // `-z`: entries end in NUL and paths are not quoted. A rename or copy
        // entry is followed by its old path, which is skipped.
        let status = run(
            self.path(),
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            None,
            None,
        )
        .await?;
        let mut paths = Vec::new();
        let mut entries = status.split('\0').filter(|e| !e.is_empty());
        while let Some(entry) = entries.next() {
            let (code, path) = (entry.get(..2).unwrap_or(""), entry.get(3..).unwrap_or(""));
            paths.push(path.to_owned());
            if code.contains('R') || code.contains('C') {
                entries.next();
            }
        }
        Ok(paths)
    }

    /// Commits every change as `identity` with `message`; hooks do not run.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when staging or committing fails.
    pub async fn commit(
        &self,
        identity: &CommitIdentity,
        message: &str,
    ) -> Result<String, GitError> {
        run(self.path(), &["add", "--all"], None, None).await?;
        let name = format!("user.name={}", identity.name);
        let email = format!("user.email={}", identity.email);
        run(
            self.path(),
            &[
                "-c",
                &name,
                "-c",
                &email,
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "--quiet",
                "--file",
                "-",
            ],
            None,
            Some(message),
        )
        .await?;
        self.head().await
    }

    /// Pushes the checkout's head to `branch` as a fast-forward. Never forced:
    /// when the branch moved, the push is refused and nothing changes.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::Rejected`] when the branch moved, or another
    /// [`GitError`] when the push fails.
    pub async fn push(&self, branch: &str) -> Result<(), GitError> {
        let refspec = format!("HEAD:refs/heads/{branch}");
        run(
            self.path(),
            &["push", "--quiet", "--no-verify", "origin", &refspec],
            self.credential.as_ref(),
            None,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use secrecy::SecretString;

    use super::*;

    /// A bare repository with one branch `feature` holding `src/a.rs`, plus
    /// the head commit. The remote is a local path: no network.
    pub(crate) async fn bare_remote(name: &str) -> (ScratchDir, CommitSha) {
        let remote = ScratchDir::new(&format!("{name}-remote")).unwrap();
        let seed = ScratchDir::new(&format!("{name}-seed")).unwrap();
        run(
            remote.path(),
            &["init", "--quiet", "--bare", "--initial-branch=main"],
            None,
            None,
        )
        .await
        .unwrap();
        let s = seed.path();
        run(
            s,
            &["init", "--quiet", "--initial-branch=feature"],
            None,
            None,
        )
        .await
        .unwrap();
        std::fs::create_dir_all(s.join("src")).unwrap();
        std::fs::write(s.join("src/a.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        let identity = CommitIdentity {
            name: "Seed".to_owned(),
            email: "seed@example.com".to_owned(),
        };
        let seeded = Checkout {
            dir: seed,
            credential: None,
        };
        let head = seeded.commit(&identity, "Seed\n").await.unwrap();
        let url = remote.path().to_string_lossy().into_owned();
        run(
            seeded.path(),
            &["push", "--quiet", &url, "HEAD:refs/heads/feature"],
            None,
            None,
        )
        .await
        .unwrap();
        (remote, CommitSha::parse(&head).unwrap())
    }

    /// The head of `feature` on a bare remote, and its last commit as
    /// `author <email>` then the message.
    pub(crate) async fn remote_feature(remote: &Path) -> (String, String) {
        let head = run(remote, &["rev-parse", "refs/heads/feature"], None, None)
            .await
            .unwrap()
            .trim()
            .to_owned();
        let log = run(
            remote,
            &["log", "-1", "--format=%an <%ae>%n%B", "feature"],
            None,
            None,
        )
        .await
        .unwrap();
        (head, log)
    }

    /// The paths the head of `feature` on a bare remote changed since
    /// `base`, sorted, and the content of `path` there.
    pub(crate) async fn remote_change(
        remote: &Path,
        base: &str,
        path: &str,
    ) -> (Vec<String>, String) {
        let names = run(
            remote,
            &["diff", "--name-only", base, "refs/heads/feature"],
            None,
            None,
        )
        .await
        .unwrap();
        let mut names: Vec<String> = names.lines().map(str::to_owned).collect();
        names.sort();
        let spec = format!("refs/heads/feature:{path}");
        let content = run(remote, &["show", &spec], None, None)
            .await
            .unwrap_or_default();
        (names, content)
    }

    /// The last commit of `feature` on a bare remote, in `git log` `format`.
    pub(crate) async fn remote_log(remote: &Path, format: &str) -> String {
        let format = format!("--format={format}");
        run(remote, &["log", "-1", &format, "feature"], None, None)
            .await
            .unwrap()
    }

    fn identity() -> CommitIdentity {
        CommitIdentity {
            name: "meneer-henk[bot]".to_owned(),
            email: "1+meneer-henk[bot]@users.noreply.github.com".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_change_is_committed_and_pushed_as_a_fast_forward() {
        let (remote, head) = bare_remote("henk-git-ff").await;
        let url = remote.path().to_string_lossy().into_owned();
        let checkout = Checkout::clone_at(
            ScratchDir::new("henk-git-ff-work").unwrap(),
            &url,
            "feature",
            &head,
            None,
        )
        .await
        .unwrap();
        assert!(checkout.changed_files().await.unwrap().is_empty());
        std::fs::write(
            checkout.path().join("src/a.rs"),
            "fn main() {\n    let x = 2;\n}\n",
        )
        .unwrap();
        std::fs::write(checkout.path().join("NEW.md"), "new\n").unwrap();
        let mut changed = checkout.changed_files().await.unwrap();
        changed.sort();
        assert_eq!(changed, ["NEW.md", "src/a.rs"]);
        let commit = checkout
            .commit(&identity(), "Address review feedback\n\nHenk-Run: r-1\n")
            .await
            .unwrap();
        checkout.push("feature").await.unwrap();

        let on_remote = run(
            remote.path(),
            &["rev-parse", "refs/heads/feature"],
            None,
            None,
        )
        .await
        .unwrap()
        .trim()
        .to_owned();
        assert_eq!(on_remote, commit);
        let log = run(
            remote.path(),
            &["log", "-1", "--format=%an <%ae>%n%B", "feature"],
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            log.starts_with("meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>"),
            "{log}"
        );
        assert!(log.contains("Henk-Run: r-1"), "{log}");
    }

    #[tokio::test]
    async fn a_commit_is_fetched_as_it_was_after_the_branch_moved() {
        let (remote, reviewed) = bare_remote("henk-git-fetch").await;
        let url = remote.path().to_string_lossy().into_owned();
        let pusher = Checkout::clone_at(
            ScratchDir::new("henk-git-fetch-push").unwrap(),
            &url,
            "feature",
            &reviewed,
            None,
        )
        .await
        .unwrap();
        std::fs::write(pusher.path().join("src/a.rs"), "moved on\n").unwrap();
        pusher.commit(&identity(), "Later\n").await.unwrap();
        pusher.push("feature").await.unwrap();

        let at = Checkout::fetch_at(
            ScratchDir::new("henk-git-fetch-at").unwrap(),
            &url,
            &reviewed,
            None,
        )
        .await
        .unwrap();
        assert_eq!(at.head().await.unwrap(), reviewed.as_str());
        assert_eq!(
            std::fs::read_to_string(at.path().join("src/a.rs")).unwrap(),
            "fn main() {\n    let x = 1;\n}\n"
        );

        let unknown = CommitSha::parse(&"1".repeat(40)).unwrap();
        let missing = Checkout::fetch_at(
            ScratchDir::new("henk-git-fetch-missing").unwrap(),
            &url,
            &unknown,
            None,
        )
        .await;
        assert!(missing.is_err());
    }

    /// A fetched commit comes alone, without its history: the planner's
    /// `bash` and `plan_workspace.md` say so and send the model to the
    /// platform's commit tools for history (#183).
    #[tokio::test]
    async fn a_fetched_commit_comes_without_its_history() {
        let (remote, seed) = bare_remote("henk-git-shallow").await;
        let url = remote.path().to_string_lossy().into_owned();
        let pusher = Checkout::clone_at(
            ScratchDir::new("henk-git-shallow-push").unwrap(),
            &url,
            "feature",
            &seed,
            None,
        )
        .await
        .unwrap();
        std::fs::write(pusher.path().join("src/a.rs"), "later\n").unwrap();
        pusher.commit(&identity(), "Later\n").await.unwrap();
        pusher.push("feature").await.unwrap();
        let later = CommitSha::parse(&pusher.head().await.unwrap()).unwrap();
        let count = |dir: std::path::PathBuf| async move {
            run(&dir, &["rev-list", "--count", "HEAD"], None, None)
                .await
                .unwrap()
                .trim()
                .to_owned()
        };
        assert_eq!(count(pusher.path().to_owned()).await, "2");

        let at = Checkout::fetch_at(
            ScratchDir::new("henk-git-shallow-at").unwrap(),
            &url,
            &later,
            None,
        )
        .await
        .unwrap();
        assert_eq!(count(at.path().to_owned()).await, "1", "no history");
    }

    #[tokio::test]
    async fn a_branch_that_moved_is_never_overwritten() {
        let (remote, head) = bare_remote("henk-git-moved").await;
        let url = remote.path().to_string_lossy().into_owned();
        let ours = Checkout::clone_at(
            ScratchDir::new("henk-git-moved-a").unwrap(),
            &url,
            "feature",
            &head,
            None,
        )
        .await
        .unwrap();
        let theirs = Checkout::clone_at(
            ScratchDir::new("henk-git-moved-b").unwrap(),
            &url,
            "feature",
            &head,
            None,
        )
        .await
        .unwrap();
        std::fs::write(theirs.path().join("THEIRS.md"), "theirs\n").unwrap();
        let their_commit = theirs.commit(&identity(), "Theirs\n").await.unwrap();
        theirs.push("feature").await.unwrap();

        std::fs::write(ours.path().join("OURS.md"), "ours\n").unwrap();
        ours.commit(&identity(), "Ours\n").await.unwrap();
        assert!(matches!(
            ours.push("feature").await,
            Err(GitError::Rejected)
        ));
        let on_remote = run(
            remote.path(),
            &["rev-parse", "refs/heads/feature"],
            None,
            None,
        )
        .await
        .unwrap()
        .trim()
        .to_owned();
        assert_eq!(on_remote, their_commit, "their work stands");

        let late = Checkout::clone_at(
            ScratchDir::new("henk-git-moved-c").unwrap(),
            &url,
            "feature",
            &head,
            None,
        )
        .await;
        assert!(
            matches!(late, Err(GitError::HeadMoved { .. })),
            "a clone of a moved branch is refused"
        );
    }

    #[tokio::test]
    async fn a_changeset_is_applied_and_links_are_not_followed() {
        use henk_domain::address::WorkspacePath;
        use std::os::unix::fs::PermissionsExt as _;

        let (remote, head) = bare_remote("henk-git-apply").await;
        let url = remote.path().to_string_lossy().into_owned();
        let checkout = Checkout::clone_at(
            ScratchDir::new("henk-git-apply-work").unwrap(),
            &url,
            "feature",
            &head,
            None,
        )
        .await
        .unwrap();
        let change = |path: &str, kind: ChangeKind| Change {
            path: WorkspacePath::parse(path).unwrap(),
            kind,
        };
        checkout
            .apply(&[
                change(
                    "tools/run.sh",
                    ChangeKind::Write {
                        content: b"#!/bin/sh\n".to_vec(),
                        executable: true,
                    },
                ),
                change("src/a.rs", ChangeKind::Delete),
            ])
            .unwrap();
        let mut changed = checkout.changed_files().await.unwrap();
        changed.sort();
        assert_eq!(changed, ["src/a.rs", "tools/run.sh"]);
        let mode = std::fs::metadata(checkout.path().join("tools/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);

        let outside = ScratchDir::new("henk-git-apply-outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), checkout.path().join("out")).unwrap();
        let refused = checkout.apply(&[change(
            "out/x",
            ChangeKind::Write {
                content: b"x".to_vec(),
                executable: false,
            },
        )]);
        assert!(
            matches!(refused, Err(GitError::Apply { .. })),
            "{refused:?}"
        );
        assert!(!outside.path().join("x").exists());
    }

    #[test]
    fn the_token_is_in_the_environment_and_never_in_the_arguments() {
        for (credential, username) in [
            (
                GitCredential::github(SecretString::from("ghs_secret123".to_owned())),
                "x-access-token",
            ),
            (
                GitCredential::gitlab(SecretString::from("glpat-secret123".to_owned())),
                "oauth2",
            ),
        ] {
            let secret = credential.token.expose_secret().to_owned();
            let command = git_command(
                Path::new("/tmp"),
                &["push", "origin", "HEAD:refs/heads/x"],
                Some(&credential),
            );
            let std = command.as_std();
            for arg in std.get_args() {
                assert!(!arg.to_string_lossy().contains(&secret));
                assert!(!arg.to_string_lossy().contains("Authorization"));
            }
            let header = std
                .get_envs()
                .find(|(k, _)| *k == "GIT_CONFIG_VALUE_0")
                .and_then(|(_, v)| v)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let encoded = STANDARD.encode(format!("{username}:{secret}"));
            assert_eq!(header, format!("Authorization: Basic {encoded}"));
            assert!(
                !format!("{credential:?}").contains(&secret),
                "a logged credential is redacted"
            );
        }
        let plain = git_command(Path::new("/tmp"), &["status"], None);
        assert!(
            plain
                .as_std()
                .get_envs()
                .all(|(k, _)| k != "GIT_CONFIG_VALUE_0"),
            "only when asked"
        );
    }
}
