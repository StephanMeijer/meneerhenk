//! How a run about an issue ends when it does not end well: a plan (§4)
//! or an execute run (§4a); and a rebase, which is about a pull request
//! (§3.6). A person's cancel gets one comment saying who; a shutdown posts
//! nothing; a failure gets one comment with the run link. Every one of
//! them ends the run's stages and records it.

use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::run::RunId;
use henk_platform::{
    IssueInfo, IssueTarget, IssueWriter, PlatformError, PlatformWriter, ReviewTarget,
};
use henk_store::{RunStatus, StageState};
use tracing::{error, info, warn};

use crate::app::App;
use crate::cancel::is_cancelled;
use crate::review::{CancelledBy, Interrupted};
use crate::stages;

/// Issue `target`, when it is one Henk works on: open, and not a pull
/// request. `doing` says what he does with issues: `plan`, `take on`.
pub(crate) async fn open_issue(
    writer: &dyn IssueWriter,
    target: &IssueTarget,
    doing: &str,
) -> anyhow::Result<IssueInfo> {
    let issue = writer.issue(target).await.context("reading the issue")?;
    let number = target.number;
    if issue.is_pull_request {
        return Err(anyhow!(
            "#{number} is a pull request; I {doing} issues, not pull requests"
        ));
    }
    if !issue.open {
        return Err(anyhow!("#{number} is closed; I {doing} open issues"));
    }
    Ok(issue)
}

/// Where a run's ending is said: the issue or pull request it is about.
#[async_trait::async_trait]
pub(crate) trait Thread: Send + Sync {
    /// Posts `body` there.
    async fn comment(&self, body: &str) -> Result<(), PlatformError>;
}

/// Who tells the issue how a plan or an execute run ended (#536).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comments {
    /// The run does: a failure, and a person's cancel, get a comment.
    Own,
    /// Whoever drove the run does, once for all the runs they drove. The
    /// run still ends in the store with its reason and posts nothing.
    Caller,
}

/// An issue, through the issue writer.
#[derive(Clone, Copy)]
pub(crate) struct IssueThread<'a> {
    pub(crate) writer: &'a Arc<dyn IssueWriter>,
    pub(crate) target: &'a IssueTarget,
    /// Whether the run says how it ended on the issue itself.
    pub(crate) comments: Comments,
}

#[async_trait::async_trait]
impl Thread for IssueThread<'_> {
    async fn comment(&self, body: &str) -> Result<(), PlatformError> {
        if self.comments == Comments::Caller {
            return Ok(());
        }
        self.writer.comment(self.target, body).await.map(|_| ())
    }
}

/// That a run failed, and why: what [`OnRun::failed`] returns, so whoever
/// drove the run can read the reason instead of parsing a message.
#[derive(Debug, thiserror::Error)]
#[error("{what} failed: {reason}")]
pub(crate) struct RunFailed {
    /// What the run did: `planning`, `execution`.
    pub(crate) what: &'static str,
    /// Why, in Henk's own words.
    pub(crate) reason: String,
}

/// A pull or merge request's conversation, through the platform writer.
#[derive(Clone, Copy)]
pub(crate) struct PullThread<'a> {
    pub(crate) writer: &'a Arc<dyn PlatformWriter>,
    pub(crate) target: &'a ReviewTarget,
}

#[async_trait::async_trait]
impl Thread for PullThread<'_> {
    async fn comment(&self, body: &str) -> Result<(), PlatformError> {
        self.writer
            .post_comment(self.target, body)
            .await
            .map(|_| ())
    }
}

/// One issue run, for its ending.
pub(crate) type OnIssue<'a> = OnRun<'a, IssueThread<'a>>;

/// One run, for its ending, said on `thread`.
pub(crate) struct OnRun<'a, T> {
    pub(crate) app: &'a App,
    /// Where it is said.
    pub(crate) thread: T,
    pub(crate) run: &'a RunId,
    pub(crate) link: &'a str,
    /// The model, for the comment's marker.
    pub(crate) model: &'a ModelId,
    /// What the run does, for the log: `planning`, `execution`.
    pub(crate) what: &'static str,
}

impl<T: Thread> OnRun<'_, T> {
    fn marker(&self, kind: MarkerKind) -> Marker {
        Marker {
            run: self.run.clone(),
            model: self.model.clone(),
            requested_by: None,
            kind: Some(kind),
            checked_by: None,
            withdrawn: None,
        }
    }

    /// Says `text` on the thread, with the run link under it. A failure to
    /// post is logged; the run's ending does not depend on it.
    pub(crate) async fn post(&self, text: &str) {
        let body = self
            .marker(MarkerKind::Reply)
            .attach(&format!("{text}\n\nRun: {}", self.link));
        if let Err(post_error) = self.thread.comment(&body).await {
            error!(%post_error, "could not post how the {} ended", self.what);
        }
    }

    /// How a run that failed ends when it was stopped rather than failing:
    /// cancelled by a person, or interrupted by a shutdown. `None` when it
    /// simply failed. It is cancelled only when it stopped for its token; a
    /// real failure after someone asked for a cancel stays a failure.
    pub(crate) async fn ended_early(&self, failure: &anyhow::Error) -> Option<anyhow::Error> {
        if is_cancelled(failure)
            && let Some(by) = self.app.cancels.cancelled_by(self.run)
        {
            return Some(self.cancelled(&by).await);
        }
        if self.app.shutdown.is_cancelled() {
            return Some(self.interrupted().await);
        }
        None
    }

    /// A person cancelled the run (#69): one comment saying who, no failure,
    /// and the run ends `cancelled`. The error is [`CancelledBy`], with the
    /// same words as before.
    pub(crate) async fn cancelled(&self, by: &str) -> anyhow::Error {
        let reason = crate::cancel::cancelled_reason(by);
        info!(run = %self.run, by, "{} {reason}", self.what);
        let body = self
            .marker(MarkerKind::Reply)
            .attach(&crate::cancel::cancelled_notice(by, self.link));
        if let Err(post_error) = self.thread.comment(&body).await {
            error!(%post_error, "could not post that the run was cancelled");
        }
        let cancelled = format!("cancelled by {by}");
        let store = &*self.app.store;
        stages::end(store, self.run, StageState::Skipped, &cancelled, &cancelled).await;
        if let Err(store_error) = store
            .finish_run(self.run, RunStatus::Cancelled, None, Some(&reason))
            .await
        {
            return store_error.into();
        }
        CancelledBy(by.to_owned()).into()
    }

    /// Stopped by Ctrl-C or a shutdown: not Henk's failure, so nothing is
    /// posted; the run ends with the reason (#7).
    pub(crate) async fn interrupted(&self) -> anyhow::Error {
        warn!(run = %self.run, "{} interrupted", self.what);
        let store = &*self.app.store;
        stages::end(
            store,
            self.run,
            StageState::Failed,
            "interrupted",
            "interrupted",
        )
        .await;
        if let Err(store_error) = store
            .finish_run(
                self.run,
                RunStatus::Failed,
                None,
                Some(&Interrupted.to_string()),
            )
            .await
        {
            return store_error.into();
        }
        Interrupted.into()
    }

    /// The run failed for `reason`: `comment` goes on the issue, with the
    /// run link under it, and the run ends `failed`.
    pub(crate) async fn failed(&self, comment: &str, reason: &str) -> anyhow::Error {
        error!(reason, "{} failed", self.what);
        let body = self
            .marker(MarkerKind::Failure)
            .attach(&format!("{comment}\n\nRun: {}", self.link));
        if let Err(post_error) = self.thread.comment(&body).await {
            error!(%post_error, "could not post the failure comment");
        }
        let store = &*self.app.store;
        stages::failed(store, self.run).await;
        if let Err(store_error) = store
            .finish_run(self.run, RunStatus::Failed, None, Some(reason))
            .await
        {
            return store_error.into();
        }
        RunFailed {
            what: self.what,
            reason: reason.to_owned(),
        }
        .into()
    }
}
