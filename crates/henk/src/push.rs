//! Committing a checked changeset as Henk and pushing it to a pull
//! request's branch as a fast-forward: what an address run (§3.5) and the
//! review loop (#284) share.
//!
//! Every guard is here, so both get all of them: the branch is read again
//! just before the push and must not have moved, Henk must still be allowed
//! to push, the checkout must change exactly the changeset's files, and a
//! cancel stops it up to the last moment. Nothing is ever forced.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use henk_domain::address::push_refusal;
use henk_domain::allowlist::Platform;
use henk_domain::commit::{CommitPerson, Email, commit_message, noreply};
use henk_domain::run::RunId;
use henk_domain::workspace::Change;
use henk_platform::ReviewTarget;
use henk_platform::address::{AddressWriter, CommitIdentity, GitCredential, PullFacts};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::App;
use crate::cancel::Cancelled;
use crate::config::AddressConfig;
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

/// Who pushes for one run, to one pull request.
pub(crate) struct Pusher<'a> {
    pub(crate) app: &'a App,
    pub(crate) writer: Arc<dyn AddressWriter>,
    pub(crate) target: &'a ReviewTarget,
    pub(crate) run: &'a RunId,
    /// Who asked, as the commit message names them.
    pub(crate) requester: &'a str,
    /// Whether the run that pushes reviews the commit itself, as the review
    /// loop does (#284): its new-commits event then starts no review. An
    /// address run's commit is a new commit like any other and is reviewed
    /// as usual (§3.5, #298). Either way it goes in [`App::own_pushes`].
    pub(crate) reviewed_by_run: bool,
}

impl Pusher<'_> {
    /// A fresh checkout of the pull request at `facts.head` that nothing
    /// ran in: only a checked changeset reaches it, and it is what Henk
    /// commits and pushes.
    /// `name` names its scratch directory.
    pub(crate) async fn checkout(
        &self,
        name: &str,
        facts: &PullFacts,
        credential: Option<GitCredential>,
    ) -> anyhow::Result<Checkout> {
        let dir = ScratchDir::new(name).context("making the checkout directory")?;
        Checkout::clone_at(
            dir,
            &facts.remote,
            &facts.push.head_ref,
            &facts.head,
            credential,
        )
        .await
        .context("checking out the pull request to push")
    }

    /// Applies the changeset to the checkout, commits as Henk with `notes`
    /// in the message, makes sure the branch has not moved, and pushes.
    /// Returns the commit, which is in [`App::own_pushes`] before it leaves
    /// when the run reviews it itself.
    pub(crate) async fn commit_and_push(
        &self,
        checkout: &Checkout,
        facts: &PullFacts,
        notes: &[String],
        changes: &[Change],
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let Some(config) = self.app.settings.address.as_ref() else {
            return Err(anyhow!("address runs are not configured"));
        };
        let henk = self.henk_identity(config).await?;
        let policy = config.trailer_policy(&self.target.repo);
        let requester = if policy.requester_coauthor || policy.requester_signoff {
            self.requester_identity(config).await?
        } else {
            None
        };
        let message = commit_message(
            notes,
            self.run,
            Some(self.requester),
            &henk,
            requester.as_ref(),
            policy,
        );
        let identity = CommitIdentity {
            name: henk.name().to_owned(),
            email: henk.email().to_string(),
        };
        // Read again just before pushing: a head that moved means someone
        // else pushed, and their work is not overwritten.
        let now = self
            .writer
            .pull_facts(self.target)
            .await
            .context("reading the pull request again")?;
        if now.head != facts.head {
            return Err(anyhow!(
                "the branch moved to {} while I worked",
                now.head.short()
            ));
        }
        if let Some(why) = push_refusal(&now.push) {
            return Err(anyhow!("I may no longer push: {why}"));
        }
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
        // commit finds it (#284, #298).
        let why = if self.reviewed_by_run {
            PushedFor::ReviewedByRun
        } else {
            PushedFor::Review
        };
        self.app.own_pushes.record(&sha, why);
        checkout
            .push(&facts.push.head_ref)
            .await
            .context("pushing")?;
        info!(commit = %sha, files = changes.len(), "pushed");
        Ok(sha)
    }

    /// Who Henk commits and signs off as: `[address.identity]`, or the
    /// App's own account.
    async fn henk_identity(&self, config: &AddressConfig) -> anyhow::Result<CommitPerson> {
        if let Some(person) = config.henk_identity().context("address.identity")? {
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
    /// email, or the platform's noreply address of the account their
    /// `[[people]]` entry gives by id, under its current login. Never a
    /// name from a comment (§2, §8.3). `None`, noted on the run, when there
    /// is no account to credit.
    async fn requester_identity(
        &self,
        config: &AddressConfig,
    ) -> anyhow::Result<Option<CommitPerson>> {
        let committer = self.app.settings.committers.get(&config.requester_id);
        if let Some(person) = committer.and_then(|c| c.commit_as.clone()) {
            return Ok(Some(person));
        }
        let platform = self.target.repo.platform();
        let account = committer.and_then(|c| match platform {
            Platform::GitHub => c.github_id,
            Platform::GitLab => c.gitlab_id,
        });
        let Some(id) = account else {
            let note = format!(
                "requester {} has no {platform} account id in [[people]]; the commit has no requester trailers",
                config.requester_id
            );
            info!("{note}");
            if let Err(error) = self.app.store.event(self.run, "info", &note).await {
                warn!(%error, "could not record the event");
            }
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
}

/// How long Henk remembers a commit it pushed: long enough for any event
/// about it to arrive.
const OWN_PUSH_TTL: Duration = Duration::from_hours(24);

/// What a commit Henk pushed is for, as the review listener treats its
/// new-commits event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushedFor {
    /// A review loop's commit: the loop reviews it in the run that pushed
    /// it, so the event starts no review (#284).
    ReviewedByRun,
    /// An address run's commit: a new commit like any other, reviewed as
    /// usual (§3.5), though Henk's own account pushed it (#298).
    Review,
}

/// The commits this process pushed and what each is for, so the event
/// about one is recognised by the commit, not by who sent it. In memory:
/// after a restart, an event about an earlier push is treated as anyone's,
/// and the sender check drops it as Henk's own.
#[derive(Debug, Default, Clone)]
pub struct OwnPushes(Arc<Mutex<HashMap<String, (Instant, PushedFor)>>>);

impl OwnPushes {
    /// Remembers `sha` as pushed by Henk for `why`, and forgets what is too
    /// old.
    pub fn record(&self, sha: &str, why: PushedFor) {
        if let Ok(mut pushed) = self.0.lock() {
            pushed.retain(|_, (at, _)| at.elapsed() < OWN_PUSH_TTL);
            pushed.insert(sha.to_ascii_lowercase(), (Instant::now(), why));
        }
    }

    /// What Henk pushed `sha` for, if Henk pushed it.
    #[must_use]
    pub fn pushed_for(&self, sha: &str) -> Option<PushedFor> {
        let pushed = self.0.lock().ok()?;
        pushed
            .get(&sha.to_ascii_lowercase())
            .filter(|(at, _)| at.elapsed() < OWN_PUSH_TTL)
            .map(|(_, why)| *why)
    }
}
