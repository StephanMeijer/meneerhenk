//! Committing a checked changeset as Henk and pushing it: to a pull
//! request's branch as a fast-forward, for the review loop (#284, §3.5), or
//! as a new branch for an issue, for an execute run (#329, §4a).
//!
//! Every guard is here, so both get all of them. A pull request's branch is
//! read again just before the push and must not have moved, and Henk must
//! still be allowed to push; a new branch must carry the configured prefix,
//! must not be the default branch and must not exist yet. The checkout must
//! change exactly the changeset's files, and a cancel stops it up to the
//! last moment. Nothing is ever forced.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::commit::{CommitPerson, Email, commit_message, noreply};
use henk_domain::identity::RequesterId;
use henk_domain::push::push_refusal;
use henk_domain::review::CommitSha;
use henk_domain::run::RunId;
use henk_domain::workspace::Change;
use henk_platform::ReviewTarget;
use henk_platform::push::{CommitIdentity, GitCredential, PullFacts, PushWriter, RepoHead};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::App;
use crate::cancel::Cancelled;
use crate::config::ExecutorConfig;
use crate::git::{Checkout, ScratchDir};

/// Stops the run when its token fired. Checked where the run would
/// otherwise go on to push, since nothing else looks at the token then.
pub(crate) fn stop_if_cancelled(cancel: &CancellationToken) -> Result<(), Cancelled> {
    if cancel.is_cancelled() {
        Err(Cancelled)
    } else {
        Ok(())
    }
}

/// The pull request was merged while the run worked (#413): nothing is
/// pushed to it, and the run ends as a success rather than a refused push.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the pull request was merged meanwhile")]
pub(crate) struct MergedMeanwhile;

/// Where a push goes.
pub(crate) enum PushTo<'a> {
    /// A pull request's own branch, as the review loop read it.
    Pull {
        /// The pull request.
        target: &'a ReviewTarget,
        /// What the loop read of it.
        facts: &'a PullFacts,
    },
    /// A new branch for an issue, off the default branch's head (#329).
    NewBranch {
        /// The branch's name, under `[execution] branch_prefix`.
        branch: &'a str,
        /// The default branch and the head it was taken from.
        base: &'a RepoHead,
    },
}

/// Who pushes for one run, to one repository.
pub(crate) struct Pusher<'a> {
    pub(crate) app: &'a App,
    pub(crate) writer: Arc<dyn PushWriter>,
    pub(crate) repo: &'a RepoRef,
    pub(crate) run: &'a RunId,
    /// Who asked, by a stable id, when Henk knows (#399): the commit names
    /// them; with `None` it names no requester at all.
    pub(crate) requester: Option<&'a RequesterId>,
}

impl Pusher<'_> {
    /// A fresh checkout of `branch` of `remote` at `head` that nothing ran
    /// in: only a checked changeset reaches it, and it is what Henk commits
    /// and pushes. `name` names its scratch directory.
    pub(crate) async fn checkout(
        &self,
        name: &str,
        remote: &str,
        branch: &str,
        head: &CommitSha,
        credential: Option<GitCredential>,
    ) -> anyhow::Result<Checkout> {
        let dir = ScratchDir::new(name).context("making the checkout directory")?;
        Checkout::clone_at(dir, remote, branch, head, credential)
            .await
            .context("checking out the branch to push")
    }

    /// [`Pusher::checkout`] of a pull request's branch at the head the loop
    /// read.
    pub(crate) async fn checkout_pull(
        &self,
        name: &str,
        facts: &PullFacts,
        credential: Option<GitCredential>,
    ) -> anyhow::Result<Checkout> {
        let (remote, branch) = (&facts.remote, &facts.push.head_ref);
        self.checkout(name, remote, branch, &facts.head, credential)
            .await
    }

    /// Applies the changeset to the checkout, commits as Henk with `subject`
    /// and `notes` in the message, makes sure Henk may push there, and
    /// pushes. Returns the commit, which is in [`App::own_pushes`] before it
    /// leaves.
    pub(crate) async fn commit_and_push(
        &self,
        checkout: &Checkout,
        to: &PushTo<'_>,
        subject: &str,
        notes: &[String],
        changes: &[Change],
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let Some(config) = self.app.settings.roles.executor.as_ref() else {
            return Err(anyhow!("pushing needs [roles.executor]"));
        };
        let henk = self.henk_identity(config).await?;
        let policy = config.trailer_policy(self.repo);
        let requester = if policy.requester_coauthor || policy.requester_signoff {
            self.requester_identity().await?
        } else {
            None
        };
        let requested_by = self.requester.map(ToString::to_string);
        let message = commit_message(
            subject,
            notes,
            self.run,
            requested_by.as_deref(),
            &henk,
            requester.as_ref(),
            policy,
        );
        let identity = CommitIdentity {
            name: henk.name().to_owned(),
            email: henk.email().to_string(),
        };
        let branch = self.may_push(checkout, to).await?;
        checkout.apply(changes).context("applying the change")?;
        let mut in_checkout = checkout
            .changed_files()
            .await
            .context("listing the changes")?;
        in_checkout.sort();
        let mut expected: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        expected.sort_unstable();
        if in_checkout != expected {
            return Err(anyhow!(
                "the checkout changed {} files where the changeset has {}",
                in_checkout.len(),
                expected.len()
            ));
        }
        let sha = checkout
            .commit(&identity, &message)
            .await
            .context("committing")?;
        // The last moment a cancel can still stop the push.
        stop_if_cancelled(cancel)?;
        // Known before the platform tells anyone, so its event about this
        // commit finds it (#284).
        self.app.own_pushes.record(&sha);
        checkout.push(branch).await.context("pushing")?;
        info!(commit = %sha, files = changes.len(), "pushed");
        Ok(sha)
    }

    /// The branch Henk pushes to, once it is sure he may.
    async fn may_push<'b>(
        &self,
        checkout: &Checkout,
        to: &'b PushTo<'_>,
    ) -> anyhow::Result<&'b str> {
        match to {
            PushTo::Pull { target, facts } => {
                // Read again just before pushing: a head that moved means
                // someone else pushed, and their work is not overwritten.
                let now = self
                    .writer
                    .pull_facts(target)
                    .await
                    .context("reading the pull request again")?;
                // Merged meanwhile: the work landed, and nothing more is
                // pushed to it (#413). A merge can move the head too.
                if now.push.merged {
                    return Err(MergedMeanwhile.into());
                }
                if now.head != facts.head {
                    return Err(anyhow!(
                        "the branch moved to {} while I worked",
                        now.head.short()
                    ));
                }
                if let Some(why) = push_refusal(&now.push) {
                    return Err(anyhow!("I may no longer push: {why}"));
                }
                Ok(facts.push.head_ref.as_str())
            }
            PushTo::NewBranch { branch, base } => {
                let prefix = self
                    .app
                    .settings
                    .execution
                    .as_ref()
                    .map(|e| e.branch_prefix.as_str())
                    .ok_or_else(|| anyhow!("a new branch needs [execution]"))?;
                if !branch.starts_with(prefix) || branch.len() == prefix.len() {
                    return Err(anyhow!("{branch} is not under {prefix}"));
                }
                if *branch == base.default_branch {
                    return Err(anyhow!("{branch} is the default branch"));
                }
                // Somebody's branch, or an earlier run's: never pushed over.
                if checkout
                    .remote_has_branch(branch)
                    .await
                    .context("looking for the branch")?
                {
                    return Err(anyhow!("the branch {branch} already exists"));
                }
                Ok(branch)
            }
        }
    }

    /// Who Henk commits and signs off as: `[address.identity]`, or the
    /// App's own account.
    async fn henk_identity(&self, config: &ExecutorConfig) -> anyhow::Result<CommitPerson> {
        if let Some(person) = config.henk_identity().context("roles.executor.identity")? {
            return Ok(person);
        }
        let identity = self
            .writer
            .commit_identity()
            .await
            .context("reading Henk's commit identity")?;
        let email = Email::parse(&identity.email).context("Henk's commit email")?;
        CommitPerson::new(&identity.name, email).context("Henk's commit name")
    }

    /// Who asked, as their trailers name them: their configured name and
    /// email, or the platform's noreply address of the account they asked
    /// with or their `[[people]]` entry gives, by id, under its current
    /// login. Never a name from a comment (§2, §8.3), and never someone
    /// who did not ask (#399). `None`, noted on the run, when nobody Henk
    /// knows asked or there is no account to credit.
    async fn requester_identity(&self) -> anyhow::Result<Option<CommitPerson>> {
        let platform = self.repo.platform();
        let Some(requester) = self.requester else {
            self.note(
                "nobody Henk knows by id asked for this run; the commit has no requester trailers",
            )
            .await;
            return Ok(None);
        };
        let committer = self.app.settings.committer_for(requester, platform);
        if let Some(person) = committer.as_ref().and_then(|c| c.commit_as.clone()) {
            return Ok(Some(person));
        }
        let account = committer.and_then(|c| match platform {
            Platform::GitHub => c.github_id,
            Platform::GitLab => c.gitlab_id,
        });
        let Some(id) = account else {
            self.note(&format!(
                "requester {requester} has no {platform} account Henk knows; the commit has no requester trailers"
            ))
            .await;
            return Ok(None);
        };
        let login = self
            .writer
            .user_login(id)
            .await
            .context("reading the requester's login")?;
        let email = match platform {
            Platform::GitHub => noreply::github(id, &login),
            Platform::GitLab => noreply::gitlab(id, &login, &self.writer.noreply_host()?),
        }
        .context("the requester's noreply address")?;
        Ok(Some(
            CommitPerson::new(&login, email).context("the requester's commit name")?,
        ))
    }

    /// A note on the run's timeline, and in the log.
    async fn note(&self, note: &str) {
        info!("{note}");
        if let Err(error) = self.app.store.event(self.run, "info", note).await {
            warn!(%error, "could not record the event");
        }
    }
}

/// How long Henk remembers a commit it pushed: long enough for any event
/// about it to arrive.
const OWN_PUSH_TTL: Duration = Duration::from_hours(24);

/// The commits this process pushed, so the event about one is recognised
/// by the commit, not by who sent it: the review loop reviews its commits
/// in the run that pushed them (#284). In memory:
/// after a restart, an event about an earlier push is treated as anyone's,
/// and the sender check drops it as Henk's own.
#[derive(Debug, Default, Clone)]
pub struct OwnPushes(Arc<Mutex<HashMap<String, Instant>>>);

impl OwnPushes {
    /// Remembers `sha` as pushed by Henk, and forgets what is too old.
    pub fn record(&self, sha: &str) {
        if let Ok(mut pushed) = self.0.lock() {
            pushed.retain(|_, at| at.elapsed() < OWN_PUSH_TTL);
            pushed.insert(sha.to_ascii_lowercase(), Instant::now());
        }
    }

    /// Whether Henk pushed `sha`.
    #[must_use]
    pub fn has(&self, sha: &str) -> bool {
        self.0.lock().is_ok_and(|pushed| {
            pushed
                .get(&sha.to_ascii_lowercase())
                .is_some_and(|at| at.elapsed() < OWN_PUSH_TTL)
        })
    }
}
