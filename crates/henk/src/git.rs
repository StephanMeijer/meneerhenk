//! A throwaway checkout of one pull request's branch, for a push: clone,
//! see what changed, commit, push. Git runs as a child
//! process with an empty environment. The credential reaches git through its
//! environment, never its arguments, so it is not in a process listing, a
//! log line or anything a model sees.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use henk_domain::review::CommitSha;
use henk_domain::workspace::{Change, ChangeKind};
use henk_platform::push::{CommitIdentity, GitCredential};
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
    /// The push was refused as not a fast-forward, or a rebase's lease
    /// no longer held (#579).
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

/// Where a rebase stands after a step (#579).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseStep {
    /// The base is already in the branch: there is nothing to replay.
    UpToDate,
    /// Every commit was replayed onto the base.
    Done,
    /// Replaying a commit stopped on conflicts.
    Conflict {
        /// The subject of the commit being replayed.
        commit: String,
        /// The files with conflicts, as git lists them.
        files: Vec<String>,
    },
}

/// A directory that is removed when dropped.
#[derive(Debug)]
pub struct ScratchDir(PathBuf);

/// Numbers this process's scratch directories, so no two share a path.
static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// How every scratch directory named `name` in this process begins:
/// `{name}-{pid}-`. Two processes, such as two test runs at once, never
/// share one (#156).
pub(crate) fn scratch_prefix(name: &str) -> String {
    format!("{name}-{}-", std::process::id())
}

impl ScratchDir {
    /// Makes an empty directory under the system temp directory, named
    /// `name` plus this process's id and a counter: readable, and never
    /// another process's or another call's (#156).
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the directory cannot be made.
    pub fn new(name: &str) -> std::io::Result<Self> {
        let n = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{}{n}", scratch_prefix(name)));
        // Only a dead process with a recycled id leaves this behind.
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

    /// Clones `branch` of `remote` into `dir` with its whole history but
    /// no file contents until they are needed (`--filter=blob:none`), checks
    /// that its head is `expected`, and fetches `base` beside it as
    /// `refs/remotes/origin/<base>`: what a rebase onto `base` needs (#579).
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when the clone or fetch fails or the head moved.
    pub async fn clone_for_rebase(
        dir: ScratchDir,
        remote: &str,
        branch: &str,
        expected: &CommitSha,
        base: &str,
        credential: Option<GitCredential>,
    ) -> Result<Self, GitError> {
        let checkout = Self { dir, credential };
        run(
            checkout.path(),
            &[
                "clone",
                "--quiet",
                "--filter=blob:none",
                "--no-tags",
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
        let refspec = format!("+refs/heads/{base}:{}", upstream(base));
        run(
            checkout.path(),
            &[
                "fetch",
                "--quiet",
                "--filter=blob:none",
                "--no-tags",
                "origin",
                &refspec,
            ],
            checkout.credential.as_ref(),
            None,
        )
        .await?;
        Ok(checkout)
    }

    /// Clones `branch` of `remote` into `dir` with its whole history but
    /// no file contents until they are needed, checks that its head is
    /// `now` (the head just read), and that `before` (the head some work was
    /// done on) is in its history: a branch that moved on from `before`,
    /// not one rewritten (#634).
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when the clone fails, when the head is not `now`
    /// (it moved again), or when `before` is not in the branch's history.
    pub async fn clone_moved(
        dir: ScratchDir,
        remote: &str,
        branch: &str,
        (before, now): (&CommitSha, &CommitSha),
        credential: Option<GitCredential>,
    ) -> Result<Self, GitError> {
        let checkout = Self { dir, credential };
        run(
            checkout.path(),
            &[
                "clone",
                "--quiet",
                "--filter=blob:none",
                "--no-tags",
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
        if !found.eq_ignore_ascii_case(now.as_str()) {
            return Err(GitError::HeadMoved {
                expected: now.as_str().to_owned(),
                found,
            });
        }
        if !matches!(checkout.is_ancestor(before.as_str()).await, Ok(true)) {
            return Err(GitError::Failed {
                command: "clone".to_owned(),
                stderr: format!(
                    "{branch} no longer holds {}; it was rewritten",
                    before.short()
                ),
            });
        }
        Ok(checkout)
    }

    /// The files that differ between commits `from` and `to` of this
    /// checkout, a renamed file under both names.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when git cannot compare them.
    pub async fn changed_between(
        &self,
        from: &CommitSha,
        to: &CommitSha,
    ) -> Result<std::collections::BTreeSet<String>, GitError> {
        let listed = run(
            self.path(),
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                from.as_str(),
                to.as_str(),
            ],
            self.credential.as_ref(),
            None,
        )
        .await?;
        Ok(listed
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Replays the branch's commits onto `base` as fetched by
    /// [`Checkout::clone_for_rebase`]. Authors are kept; `identity` is the
    /// committer. Hooks do not run and nothing is signed.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when git fails other than on a conflict, or
    /// when a conflict has no markers to resolve: a file deleted, renamed
    /// or added on one side only, or a binary file.
    pub async fn rebase(
        &self,
        identity: &CommitIdentity,
        base: &str,
    ) -> Result<RebaseStep, GitError> {
        let upstream = upstream(base);
        if self.is_ancestor(&upstream).await? {
            return Ok(RebaseStep::UpToDate);
        }
        self.rebase_step(identity, &["rebase", "--no-autosquash", &upstream])
            .await
    }

    /// Stages the resolved `files` and goes on replaying. A commit that the
    /// resolution left empty is dropped, as git drops one that was already
    /// in the base.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when git fails other than on a conflict, or
    /// on a conflict that cannot be resolved by editing markers.
    pub async fn rebase_continue(
        &self,
        identity: &CommitIdentity,
        files: &[String],
    ) -> Result<RebaseStep, GitError> {
        let mut listed = files.join("\0");
        listed.push('\0');
        run(
            self.path(),
            &[
                "--literal-pathspecs",
                "add",
                "--all",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            None,
            Some(&listed),
        )
        .await?;
        let staged = run(
            self.path(),
            &["diff", "--cached", "--name-only"],
            None,
            None,
        )
        .await?;
        let next = if staged.trim().is_empty() {
            "--skip"
        } else {
            "--continue"
        };
        self.rebase_step(identity, &["rebase", next]).await
    }

    /// Stops a rebase and puts the branch back as it was.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when git cannot.
    pub async fn rebase_abort(&self) -> Result<(), GitError> {
        run(self.path(), &["rebase", "--abort"], None, None).await?;
        Ok(())
    }

    /// How many commits the branch has that `base` does not.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when git cannot say.
    pub async fn commits_ahead(&self, base: &str) -> Result<usize, GitError> {
        let range = format!("{}..HEAD", upstream(base));
        let count = run(self.path(), &["rev-list", "--count", &range], None, None).await?;
        Ok(count.trim().parse().unwrap_or(0))
    }

    /// Of `files`, those that still hold a conflict marker line.
    #[must_use]
    pub fn still_conflicted(&self, files: &[String]) -> Vec<String> {
        files
            .iter()
            .filter(|file| {
                std::fs::read(self.path().join(file)).is_ok_and(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .lines()
                        .any(is_conflict_marker)
                })
            })
            .cloned()
            .collect()
    }

    /// Whether `commit` is already in the branch.
    async fn is_ancestor(&self, commit: &str) -> Result<bool, GitError> {
        let status = git_command(
            self.path(),
            &["merge-base", "--is-ancestor", commit, "HEAD"],
            None,
        )
        .status()
        .await?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(GitError::Failed {
                command: "merge-base".to_owned(),
                stderr: format!("cannot compare {commit} with HEAD ({status})"),
            }),
        }
    }

    /// Runs one rebase command as `identity`: done, or stopped on the
    /// conflicts it lists. Missing file contents are fetched on the way,
    /// so it has the credential.
    async fn rebase_step(
        &self,
        identity: &CommitIdentity,
        args: &[&str],
    ) -> Result<RebaseStep, GitError> {
        let name = format!("user.name={}", identity.name);
        let email = format!("user.email={}", identity.email);
        let mut full = vec![
            "-c",
            &name,
            "-c",
            &email,
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.editor=true",
        ];
        full.extend_from_slice(args);
        let Err(error) = run(self.path(), &full, self.credential.as_ref(), None).await else {
            return Ok(RebaseStep::Done);
        };
        let listed = run(self.path(), &["ls-files", "--unmerged", "-z"], None, None).await?;
        let unmerged = unmerged_stages(&listed);
        if unmerged.is_empty() {
            return Err(error);
        }
        for (file, stages) in &unmerged {
            // Stage 2 is the base's side, 3 the branch's: a file changed
            // on both sides. Anything else is added, deleted or renamed on
            // one side, and no marker says what is right.
            if !(stages.contains(&2) && stages.contains(&3)) {
                return Err(GitError::Failed {
                    command: "rebase".to_owned(),
                    stderr: format!(
                        "{file} was deleted, renamed or added on one side only; \
                         that conflict needs a human"
                    ),
                });
            }
            // Git writes no markers into a binary file.
            if self.still_conflicted(std::slice::from_ref(file)).is_empty() {
                return Err(GitError::Failed {
                    command: "rebase".to_owned(),
                    stderr: format!(
                        "{file} conflicts without conflict markers (a binary file?); \
                         that conflict needs a human"
                    ),
                });
            }
        }
        let files: Vec<String> = unmerged.into_iter().map(|(file, _)| file).collect();
        let commit = run(
            self.path(),
            &["log", "-1", "--format=%s", "REBASE_HEAD"],
            None,
            None,
        )
        .await
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
        Ok(RebaseStep::Conflict { commit, files })
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

    /// The unified diff of `changes` against the commit checked out, for a
    /// reviewer who reads what an agent changed before anything is pushed
    /// (#535). The tree is first put back to that commit, so each call
    /// shows the whole changeset and not what is new since the last one.
    /// New files, deletions, renames and mode changes are in it, binary
    /// files as a line saying they differ.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when a change cannot be applied or git fails.
    pub async fn diff_of(&self, changes: &[Change]) -> Result<String, GitError> {
        run(
            self.path(),
            &["reset", "--quiet", "--hard", "HEAD"],
            None,
            None,
        )
        .await?;
        run(
            self.path(),
            &["clean", "--quiet", "--force", "-d"],
            None,
            None,
        )
        .await?;
        self.apply(changes)?;
        run(self.path(), &["add", "--all"], None, None).await?;
        run(
            self.path(),
            &[
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--find-renames",
                "HEAD",
            ],
            None,
            None,
        )
        .await
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
    /// Henk's own pushes commit path by path ([`Checkout::commit_paths`]);
    /// this one makes the tests' history.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when staging or committing fails.
    #[cfg(test)]
    pub async fn commit(
        &self,
        identity: &CommitIdentity,
        message: &str,
    ) -> Result<String, GitError> {
        run(self.path(), &["add", "--all"], None, None).await?;
        self.commit_staged(identity, message).await
    }

    /// Commits the changes to `paths` alone, added, changed and deleted, as
    /// `identity` with `message`; the rest stays uncommitted for the next
    /// commit (#400). Paths are taken literally, never as patterns.
    ///
    /// # Errors
    ///
    /// Returns [`GitError`] when staging or committing fails.
    pub async fn commit_paths(
        &self,
        identity: &CommitIdentity,
        message: &str,
        paths: &[String],
    ) -> Result<String, GitError> {
        let mut listed = paths.join("\0");
        listed.push('\0');
        run(
            self.path(),
            &[
                "--literal-pathspecs",
                "add",
                "--all",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            None,
            Some(&listed),
        )
        .await?;
        self.commit_staged(identity, message).await
    }

    /// Commits what is staged; hooks do not run.
    async fn commit_staged(
        &self,
        identity: &CommitIdentity,
        message: &str,
    ) -> Result<String, GitError> {
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

    /// The subjects of the last `n` commits, newest first: how the
    /// repository words its commits, for the executor (#400). Empty when
    /// git cannot say.
    pub async fn recent_subjects(&self, n: usize) -> Vec<String> {
        let count = n.to_string();
        run(
            self.path(),
            &["log", "-n", &count, "--format=%s"],
            None,
            None,
        )
        .await
        .map(|log| log.lines().map(str::to_owned).collect())
        .unwrap_or_default()
    }

    /// Whether `origin` has a branch named `branch` (#329).
    ///
    /// # Errors
    ///
    /// When git cannot reach the remote.
    pub async fn remote_has_branch(&self, branch: &str) -> Result<bool, GitError> {
        let refname = format!("refs/heads/{branch}");
        let listed = run(
            self.path(),
            &["ls-remote", "--heads", "origin", &refname],
            self.credential.as_ref(),
            None,
        )
        .await?;
        Ok(!listed.trim().is_empty())
    }

    /// Pushes the checkout's head to `branch` as a fast-forward, or as a new
    /// branch when there is none by that name. Never forced: when the branch
    /// moved, the push is refused and nothing changes.
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

    /// Pushes the rebased head to `branch` in place of `old`, the head Henk
    /// read before the rebase (#579): forced, but only while the branch is
    /// still at `old`, so a commit pushed meanwhile is never lost.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::Rejected`] when the branch is no longer at
    /// `old`, or another [`GitError`] when the push fails.
    pub async fn push_with_lease(&self, branch: &str, old: &CommitSha) -> Result<(), GitError> {
        let lease = format!("--force-with-lease=refs/heads/{branch}:{}", old.as_str());
        let refspec = format!("HEAD:refs/heads/{branch}");
        run(
            self.path(),
            &["push", "--quiet", "--no-verify", &lease, "origin", &refspec],
            self.credential.as_ref(),
            None,
        )
        .await?;
        Ok(())
    }
}

/// Where a fetched base branch is kept in a checkout.
fn upstream(base: &str) -> String {
    format!("refs/remotes/origin/{base}")
}

/// The unmerged files in the output of `git ls-files --unmerged -z`, each
/// with the index stages it has, in the order git lists them.
fn unmerged_stages(listing: &str) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in listing.split('\0').filter(|e| !e.is_empty()) {
        // `<mode> <object> <stage>\t<path>`
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let Some(stage) = meta.rsplit(' ').next().and_then(|s| s.parse::<u8>().ok()) else {
            continue;
        };
        match files.last_mut() {
            Some((last, stages)) if last == path => stages.push(stage),
            _ => files.push((path.to_owned(), vec![stage])),
        }
    }
    files
}

/// Whether a line is one git writes around a conflict. A bare `=======`
/// is left out: it is also a Markdown heading underline.
fn is_conflict_marker(line: &str) -> bool {
    ["<<<<<<<", ">>>>>>>", "|||||||"].iter().any(|marker| {
        line.strip_prefix(marker)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
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

    /// A bare repository whose `feature` branched off `main` and changed
    /// `src/b.rs`, after which `main` changed `src/a.rs`; with `clash`,
    /// `feature` changed the same line of `src/a.rs` too. The head of
    /// `feature` is returned.
    pub(crate) async fn diverged_remote(name: &str, clash: bool) -> (ScratchDir, CommitSha) {
        let remote = ScratchDir::new(&format!("{name}-remote")).unwrap();
        let seed = ScratchDir::new(&format!("{name}-seed")).unwrap();
        let s = seed.path().to_path_buf();
        let git = |args: Vec<String>| {
            let s = s.clone();
            async move {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                run(&s, &args, None, None).await.unwrap()
            }
        };
        let words = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        run(
            remote.path(),
            &["init", "--quiet", "--bare", "--initial-branch=main"],
            None,
            None,
        )
        .await
        .unwrap();
        git(words("init --quiet --initial-branch=main")).await;
        let seeded = Checkout {
            dir: seed,
            credential: None,
        };
        let author = CommitIdentity {
            name: "Seed".to_owned(),
            email: "seed@example.com".to_owned(),
        };
        std::fs::create_dir_all(s.join("src")).unwrap();
        std::fs::write(s.join("src/a.rs"), "let x = 1;\n").unwrap();
        seeded.commit(&author, "Seed\n").await.unwrap();
        git(words("checkout --quiet -b feature")).await;
        std::fs::write(s.join("src/b.rs"), "let y = 2;\n").unwrap();
        if clash {
            std::fs::write(s.join("src/a.rs"), "let x = 3;\n").unwrap();
        }
        let head = seeded.commit(&author, "Add b\n").await.unwrap();
        git(words("checkout --quiet main")).await;
        std::fs::write(s.join("src/a.rs"), "let x = 2;\n").unwrap();
        seeded.commit(&author, "Change a\n").await.unwrap();
        let url = remote.path().to_string_lossy().into_owned();
        for branch in ["main", "feature"] {
            git(vec![
                "push".to_owned(),
                "--quiet".to_owned(),
                url.clone(),
                format!("refs/heads/{branch}:refs/heads/{branch}"),
            ])
            .await;
        }
        (remote, CommitSha::parse(&head).unwrap())
    }

    async fn rebase_checkout(name: &str, remote: &ScratchDir, head: &CommitSha) -> Checkout {
        Checkout::clone_for_rebase(
            ScratchDir::new(name).unwrap(),
            &remote.path().to_string_lossy(),
            "feature",
            head,
            "main",
            None,
        )
        .await
        .unwrap()
    }

    fn henk() -> CommitIdentity {
        CommitIdentity {
            name: "Henk".to_owned(),
            email: "henk@example.com".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_branch_behind_its_base_is_rebased_and_pushed_with_a_lease() {
        let (remote, head) = diverged_remote("git-rebase-clean", false).await;
        let checkout = rebase_checkout("git-rebase-clean-co", &remote, &head).await;
        assert_eq!(
            checkout.rebase(&henk(), "main").await.unwrap(),
            RebaseStep::Done
        );
        assert_eq!(checkout.commits_ahead("main").await.unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(checkout.path().join("src/a.rs")).unwrap(),
            "let x = 2;\n"
        );
        checkout.push_with_lease("feature", &head).await.unwrap();
        let log = remote_branch_log(remote.path(), "feature", "%an %cn %s").await;
        assert_eq!(log.trim(), "Seed Henk Add b", "the author is kept");
        assert_eq!(
            checkout.rebase(&henk(), "main").await.unwrap(),
            RebaseStep::UpToDate
        );
    }

    #[tokio::test]
    async fn a_lease_refuses_a_branch_that_moved() {
        let (remote, head) = diverged_remote("git-rebase-lease", false).await;
        let checkout = rebase_checkout("git-rebase-lease-co", &remote, &head).await;
        checkout.rebase(&henk(), "main").await.unwrap();
        let other = rebase_checkout("git-rebase-lease-other", &remote, &head).await;
        std::fs::write(other.path().join("c.txt"), "theirs\n").unwrap();
        other.commit(&henk(), "Theirs\n").await.unwrap();
        other.push("feature").await.unwrap();
        assert!(matches!(
            checkout.push_with_lease("feature", &head).await,
            Err(GitError::Rejected)
        ));
        let log = remote_branch_log(remote.path(), "feature", "%s").await;
        assert_eq!(log.trim(), "Theirs", "their commit is still there");
    }

    #[tokio::test]
    async fn a_conflict_stops_the_rebase_until_it_is_resolved() {
        let (remote, head) = diverged_remote("git-rebase-conflict", true).await;
        let checkout = rebase_checkout("git-rebase-conflict-co", &remote, &head).await;
        let step = checkout.rebase(&henk(), "main").await.unwrap();
        let files = vec!["src/a.rs".to_owned()];
        assert_eq!(
            step,
            RebaseStep::Conflict {
                commit: "Add b".to_owned(),
                files: files.clone()
            }
        );
        assert_eq!(checkout.still_conflicted(&files), files);
        std::fs::write(checkout.path().join("src/a.rs"), "let x = 3;\n").unwrap();
        assert!(checkout.still_conflicted(&files).is_empty());
        assert_eq!(
            checkout.rebase_continue(&henk(), &files).await.unwrap(),
            RebaseStep::Done
        );
        assert_eq!(checkout.commits_ahead("main").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_conflict_without_markers_is_not_left_to_the_executor() {
        // Modify/delete: the branch edits src/a.rs, the base deletes it.
        let (remote, head) = diverged_remote("git-rebase-deleted", true).await;
        let url = remote.path().to_string_lossy().into_owned();
        let other = ScratchDir::new("git-rebase-deleted-other").unwrap();
        run(other.path(), &["clone", "--quiet", &url, "."], None, None)
            .await
            .unwrap();
        let other = Checkout {
            dir: other,
            credential: None,
        };
        run(other.path(), &["rm", "--quiet", "src/a.rs"], None, None)
            .await
            .unwrap();
        other.commit(&henk(), "Delete a\n").await.unwrap();
        other.push("main").await.unwrap();
        let checkout = rebase_checkout("git-rebase-deleted-co", &remote, &head).await;
        let error = checkout.rebase(&henk(), "main").await.unwrap_err();
        assert!(error.to_string().contains("src/a.rs"), "{error}");
    }

    #[tokio::test]
    async fn a_binary_conflict_is_not_left_to_the_executor() {
        let (remote, head) = diverged_remote("git-rebase-binary", false).await;
        let url = remote.path().to_string_lossy().into_owned();
        let other = ScratchDir::new("git-rebase-binary-other").unwrap();
        run(other.path(), &["clone", "--quiet", &url, "."], None, None)
            .await
            .unwrap();
        let other = Checkout {
            dir: other,
            credential: None,
        };
        std::fs::write(other.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        other.commit(&henk(), "Add blob\n").await.unwrap();
        other.push("main").await.unwrap();
        // The branch adds the same path with other bytes.
        let branch = ScratchDir::new("git-rebase-binary-branch").unwrap();
        run(
            branch.path(),
            &["clone", "--quiet", "--branch", "feature", &url, "."],
            None,
            None,
        )
        .await
        .unwrap();
        let branch = Checkout {
            dir: branch,
            credential: None,
        };
        std::fs::write(branch.path().join("blob.bin"), [0u8, 9, 8, 7]).unwrap();
        branch.commit(&henk(), "Branch blob\n").await.unwrap();
        let moved = branch.head().await.unwrap();
        branch.push("feature").await.unwrap();
        let _ = head;
        let checkout = rebase_checkout(
            "git-rebase-binary-co",
            &remote,
            &CommitSha::parse(&moved).unwrap(),
        )
        .await;
        let error = checkout.rebase(&henk(), "main").await.unwrap_err();
        assert!(error.to_string().contains("blob.bin"), "{error}");
    }

    #[test]
    fn unmerged_files_are_listed_with_their_stages() {
        let listing = "100644 aaa 1\ta.rs\x00100644 bbb 2\ta.rs\x00100644 ccc 3\ta.rs\x00\
                       100644 ddd 2\tb with space.rs\x00";
        assert_eq!(
            unmerged_stages(listing),
            vec![
                ("a.rs".to_owned(), vec![1, 2, 3]),
                ("b with space.rs".to_owned(), vec![2]),
            ]
        );
    }

    #[tokio::test]
    async fn an_aborted_rebase_leaves_the_branch_as_it_was() {
        let (remote, head) = diverged_remote("git-rebase-abort", true).await;
        let checkout = rebase_checkout("git-rebase-abort-co", &remote, &head).await;
        checkout.rebase(&henk(), "main").await.unwrap();
        checkout.rebase_abort().await.unwrap();
        assert_eq!(checkout.head().await.unwrap(), head.as_str());
    }

    #[test]
    fn conflict_markers_are_told_from_heading_underlines() {
        for line in ["<<<<<<< HEAD", ">>>>>>> abc (Add b)", "|||||||", "<<<<<<<"] {
            assert!(is_conflict_marker(line), "{line}");
        }
        for line in ["=======", "<<<<<<<<", "a <<<<<<< b", "Title"] {
            assert!(!is_conflict_marker(line), "{line}");
        }
    }

    /// Two scratch directories with one name get their own paths, both
    /// under this process's prefix, and dropping one leaves the other and
    /// its contents alone (#156).
    #[test]
    fn scratch_dirs_with_one_name_do_not_share_a_path() {
        let name = "scratch-unique";
        let prefix = scratch_prefix(name);
        let first = ScratchDir::new(name).unwrap();
        let kept = first.path().join("kept.txt");
        std::fs::write(&kept, "still here").unwrap();
        let second = ScratchDir::new(name).unwrap();
        assert_ne!(first.path(), second.path());
        for dir in [&first, &second] {
            let base = dir.path().file_name().unwrap().to_string_lossy();
            assert!(base.starts_with(&prefix), "{base} lacks {prefix}");
        }
        let second_path = second.path().to_path_buf();
        drop(second);
        assert!(!second_path.exists());
        assert!(first.path().is_dir());
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "still here");
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
        remote_branch_log(remote, "feature", format).await
    }

    /// The last commit of `branch` on a bare remote, in `git log` `format`.
    pub(crate) async fn remote_branch_log(remote: &Path, branch: &str, format: &str) -> String {
        let format = format!("--format={format}");
        let branch = format!("refs/heads/{branch}");
        run(remote, &["log", "-1", &format, &branch], None, None)
            .await
            .unwrap()
    }

    /// The last `n` commits of `branch` on a bare remote, newest first, in
    /// `git log` `format`, one per entry.
    pub(crate) async fn remote_commits(
        remote: &Path,
        branch: &str,
        n: usize,
        format: &str,
    ) -> Vec<String> {
        let format = format!("--format={format}%x00");
        let (branch, count) = (format!("refs/heads/{branch}"), n.to_string());
        run(remote, &["log", "-n", &count, &format, &branch], None, None)
            .await
            .unwrap()
            .split('\0')
            .map(|c| c.trim_matches('\n').to_owned())
            .filter(|c| !c.is_empty())
            .collect()
    }

    /// The content of `path` at the head of `branch` on a bare remote.
    pub(crate) async fn remote_branch_file(remote: &Path, branch: &str, path: &str) -> String {
        let spec = format!("refs/heads/{branch}:{path}");
        run(remote, &["show", &spec], None, None).await.unwrap()
    }

    /// The branches of a bare remote, sorted.
    pub(crate) async fn remote_branches(remote: &Path) -> Vec<String> {
        let listed = run(
            remote,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
            None,
            None,
        )
        .await
        .unwrap();
        let mut branches: Vec<String> = listed.lines().map(str::to_owned).collect();
        branches.sort();
        branches
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
            .commit(&identity(), "Fix review findings\n\nHenk-Run: r-1\n")
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
    async fn one_change_is_committed_path_by_path_as_several_commits() {
        let (remote, head) = bare_remote("henk-git-split").await;
        let url = remote.path().to_string_lossy().into_owned();
        let checkout = Checkout::clone_at(
            ScratchDir::new("henk-git-split-work").unwrap(),
            &url,
            "feature",
            &head,
            None,
        )
        .await
        .unwrap();
        assert_eq!(checkout.recent_subjects(20).await, ["Seed"]);
        std::fs::write(checkout.path().join("NEW.md"), "new\n").unwrap();
        std::fs::write(checkout.path().join("*.md"), "a star\n").unwrap();
        std::fs::remove_file(checkout.path().join("src/a.rs")).unwrap();
        checkout
            .commit_paths(&identity(), "docs: add notes\n", &["NEW.md".to_owned()])
            .await
            .unwrap();
        let mut left = checkout.changed_files().await.unwrap();
        left.sort();
        // A path is literal: `*.md` did not take NEW.md, nor NEW.md `*.md`.
        assert_eq!(left, ["*.md", "src/a.rs"]);
        checkout
            .commit_paths(
                &identity(),
                "refactor: drop a.rs\n",
                &["src/a.rs".to_owned(), "*.md".to_owned()],
            )
            .await
            .unwrap();
        assert!(checkout.changed_files().await.unwrap().is_empty());
        checkout.push("feature").await.unwrap();
        assert_eq!(
            remote_commits(remote.path(), "feature", 3, "%s|%an").await,
            [
                "refactor: drop a.rs|meneer-henk[bot]",
                "docs: add notes|meneer-henk[bot]",
                "Seed|Seed",
            ]
        );
        assert_eq!(
            checkout.recent_subjects(2).await,
            ["refactor: drop a.rs", "docs: add notes"]
        );
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

    /// Commits `files` to a clone of the remote's `feature` and pushes the
    /// result to `branch`: somebody's merge, as far as a run can tell. A
    /// file with `None` is removed. Returns the commit.
    pub(crate) async fn push_files(
        remote: &Path,
        branch: &str,
        name: &str,
        files: &[(&str, Option<&str>)],
    ) -> CommitSha {
        let url = remote.to_string_lossy().into_owned();
        let (head, _) = remote_feature(remote).await;
        let later = Checkout::clone_at(
            ScratchDir::new(&format!("{name}-later")).unwrap(),
            &url,
            "feature",
            &CommitSha::parse(&head).unwrap(),
            None,
        )
        .await
        .unwrap();
        for (path, content) in files {
            let target = later.path().join(path);
            match content {
                Some(content) => {
                    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                    std::fs::write(&target, content).unwrap();
                }
                None => std::fs::remove_file(&target).unwrap(),
            }
        }
        let sha = later
            .commit(
                &CommitIdentity {
                    name: "Later".to_owned(),
                    email: "later@example.com".to_owned(),
                },
                "Later\n",
            )
            .await
            .unwrap();
        later.push(branch).await.unwrap();
        CommitSha::parse(&sha).unwrap()
    }

    /// #634: a default branch that moved on is taken at its new head, with
    /// the files that changed between the two heads, a renamed one under both
    /// names and a deleted one too.
    #[tokio::test]
    async fn a_branch_that_moved_on_is_taken_again_and_says_which_files_changed() {
        let (remote, before) = bare_remote("henk-git-moved-on").await;
        let url = remote.path().to_string_lossy().into_owned();
        let first = push_files(
            remote.path(),
            "feature",
            "henk-git-moved-on-1",
            &[("src/a.rs", Some("changed\n")), ("src/old.rs", Some("x\n"))],
        )
        .await;
        let now = push_files(
            remote.path(),
            "feature",
            "henk-git-moved-on-2",
            &[
                ("src/a.rs", None),
                ("src/old.rs", None),
                ("src/renamed.rs", Some("x\n")),
            ],
        )
        .await;

        let checkout = Checkout::clone_moved(
            ScratchDir::new("henk-git-moved-on-co").unwrap(),
            &url,
            "feature",
            (&before, &now),
            None,
        )
        .await
        .unwrap();
        assert_eq!(checkout.head().await.unwrap(), now.as_str());
        let since_first = checkout.changed_between(&first, &now).await.unwrap();
        assert_eq!(
            since_first.into_iter().collect::<Vec<_>>(),
            ["src/a.rs", "src/old.rs", "src/renamed.rs"]
        );
        let since_before = checkout.changed_between(&before, &now).await.unwrap();
        assert_eq!(
            since_before.into_iter().collect::<Vec<_>>(),
            ["src/a.rs", "src/renamed.rs"],
            "old.rs did not exist before, so it is not a change from there"
        );
        assert!(
            checkout
                .changed_between(&now, &now)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// #634: only a branch that moved ahead is replayed onto: one that moved
    /// again since it was read, or that no longer holds the head the work began
    /// on (it was rewritten), is refused.
    #[tokio::test]
    async fn a_branch_that_moved_again_or_was_rewritten_is_not_taken_again() {
        let (remote, before) = bare_remote("henk-git-moved-refused").await;
        let url = remote.path().to_string_lossy().into_owned();
        let read = push_files(
            remote.path(),
            "feature",
            "henk-git-moved-refused-1",
            &[("src/b.rs", Some("b\n"))],
        )
        .await;
        push_files(
            remote.path(),
            "feature",
            "henk-git-moved-refused-2",
            &[("src/c.rs", Some("c\n"))],
        )
        .await;
        let again = Checkout::clone_moved(
            ScratchDir::new("henk-git-moved-refused-a").unwrap(),
            &url,
            "feature",
            (&before, &read),
            None,
        )
        .await;
        assert!(
            matches!(again, Err(GitError::HeadMoved { .. })),
            "{again:?}"
        );

        let (now, _) = remote_feature(remote.path()).await;
        let unrelated = CommitSha::parse(&"1".repeat(40)).unwrap();
        let rewritten = Checkout::clone_moved(
            ScratchDir::new("henk-git-moved-refused-b").unwrap(),
            &url,
            "feature",
            (&unrelated, &CommitSha::parse(&now).unwrap()),
            None,
        )
        .await;
        let Err(GitError::Failed { stderr, .. }) = rewritten else {
            panic!("expected a refusal, got {rewritten:?}");
        };
        assert!(stderr.contains("rewritten"), "{stderr}");
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
        use henk_domain::path::WorkspacePath;
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

    #[tokio::test]
    async fn the_diff_of_a_changeset_holds_every_kind_of_change_and_starts_over_each_call() {
        use henk_domain::diff::{FileStatus, ReviewDiff};
        use henk_domain::path::WorkspacePath;

        let (remote, head) = bare_remote("henk-git-diff").await;
        let url = remote.path().to_string_lossy().into_owned();
        let checkout = Checkout::clone_at(
            ScratchDir::new("henk-git-diff-work").unwrap(),
            &url,
            "feature",
            &head,
            None,
        )
        .await
        .unwrap();
        let write = |path: &str, content: &[u8], executable: bool| Change {
            path: WorkspacePath::parse(path).unwrap(),
            kind: ChangeKind::Write {
                content: content.to_vec(),
                executable,
            },
        };
        let diff = |text: &str| ReviewDiff::from_unified(text);
        let status = |diff: &ReviewDiff, path: &str| {
            diff.files()
                .iter()
                .find(|f| f.path == path)
                .map(|f| f.status)
        };

        // Modified, new, an executable, a file with a space and a non-ASCII
        // name, and a binary one.
        let first = checkout
            .diff_of(&[
                write("src/a.rs", b"fn main() {\n    let x = 2;\n}\n", false),
                write("src/csv.rs", b"pub fn export() {}\n", false),
                write("tools/run.sh", b"#!/bin/sh\n", true),
                write("docs/a b.md", b"# Notes\n", false),
                write("docs/caf\u{e9}.md", b"# Caf\xc3\xa9\n", false),
                write("assets/logo.bin", &[0, 159, 146, 150, 0, 1, 2], false),
            ])
            .await
            .unwrap();
        let parsed = diff(&first);
        assert_eq!(
            status(&parsed, "src/a.rs"),
            Some(FileStatus::Modified),
            "{first}"
        );
        for added in [
            "src/csv.rs",
            "tools/run.sh",
            "docs/a b.md",
            "docs/caf\u{e9}.md",
            "assets/logo.bin",
        ] {
            assert_eq!(
                status(&parsed, added),
                Some(FileStatus::Added),
                "{added}: {first}"
            );
        }
        assert!(first.contains("new file mode 100755"), "{first}");
        assert!(first.contains("-    let x = 1;"), "{first}");
        assert!(first.contains("+    let x = 2;"), "{first}");
        assert!(first.contains("Binary files"), "{first}");

        // A second call shows its own changeset alone: a deletion, and a
        // rename (same content under a new name).
        let second = checkout
            .diff_of(&[
                Change {
                    path: WorkspacePath::parse("src/a.rs").unwrap(),
                    kind: ChangeKind::Delete,
                },
                write("src/main.rs", b"fn main() {\n    let x = 1;\n}\n", false),
            ])
            .await
            .unwrap();
        let parsed = diff(&second);
        assert_eq!(parsed.files().len(), 1, "{second}");
        assert_eq!(
            status(&parsed, "src/main.rs"),
            Some(FileStatus::Renamed),
            "{second}"
        );
        assert!(!second.contains("csv.rs"), "{second}");

        // Deleted outright, and nothing changed at all.
        let deleted = checkout
            .diff_of(&[Change {
                path: WorkspacePath::parse("src/a.rs").unwrap(),
                kind: ChangeKind::Delete,
            }])
            .await
            .unwrap();
        assert_eq!(
            status(&diff(&deleted), "src/a.rs"),
            Some(FileStatus::Removed),
            "{deleted}"
        );
        assert_eq!(checkout.diff_of(&[]).await.unwrap(), "");
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
