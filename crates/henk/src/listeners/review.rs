//! Starts reviews (§3.1): pull request changes, review commands, direct requests.

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::queue::Decision;
use henk_domain::review::is_review_request;
use henk_events::{CommentKind, Event, EventKind, Handled, Listener, PullRequestAction};
use henk_platform::{PullRequestState, ReviewTarget};
use tracing::instrument;

use super::Writers;
use super::filter::rejected;
use crate::coordinator::Coordinator;
use crate::review::ReviewRequest;

/// Turns events into review requests for the coordinator.
pub struct ReviewListener {
    coordinator: Arc<Coordinator>,
    writers: Arc<dyn Writers>,
}

impl ReviewListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>, writers: Arc<dyn Writers>) -> Self {
        Self {
            coordinator,
            writers,
        }
    }

    fn submit(&self, request: ReviewRequest, commit: henk_domain::review::CommitSha) -> Handled {
        let (decision, run) = self.coordinator.submit_review(request, commit);
        match decision {
            Decision::Start => Handled::Started(run),
            Decision::Join => Handled::Joined(run),
            Decision::Supersede => Handled::Superseded(run),
        }
    }

    async fn head_of(
        &self,
        target: &ReviewTarget,
    ) -> Result<henk_domain::review::CommitSha, Handled> {
        let writer = self
            .writers
            .writer(target.platform())
            .map_err(|e| Handled::Ignored(e.to_string()))?;
        match writer.pull_request(target).await {
            Ok(info) if info.state == PullRequestState::Open => Ok(info.head),
            Ok(_) => Err(Handled::Ignored("pull request is not open".to_owned())),
            Err(error) => Err(Handled::Failed(format!(
                "could not read the pull request: {error}"
            ))),
        }
    }
}

#[async_trait::async_trait]
impl Listener for ReviewListener {
    fn name(&self) -> &'static str {
        "review"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let settings = &self.coordinator.app().settings;
        match &event.kind {
            EventKind::PullRequest {
                repo,
                number,
                action,
                head,
                draft,
                sender,
            } => {
                if let Some(reason) = rejected(settings, repo, sender, None) {
                    return Handled::Ignored(reason);
                }
                // GitLab drafts are never reviewed (§3.1); GitHub drafts only when configured.
                let drafts_reviewed =
                    repo.platform() == Platform::GitHub && settings.review.github_drafts;
                if *draft && !drafts_reviewed {
                    return Handled::Ignored("draft; not reviewed until marked ready".to_owned());
                }
                let trigger = match action {
                    PullRequestAction::Opened => "opened",
                    PullRequestAction::Reopened => "reopened",
                    PullRequestAction::Synchronized => "new commits",
                };
                let request = ReviewRequest {
                    target: ReviewTarget {
                        repo: repo.clone(),
                        number: *number,
                    },
                    commit: Some(head.clone()),
                    trigger: trigger.to_owned(),
                    requester: Some(sender.login.clone()),
                    acknowledge: None,
                    run: None,
                    submitted_at: None,
                    queued_check: None,
                };
                self.submit(request, head.clone())
            }
            EventKind::Comment {
                repo,
                number,
                body,
                comment_id,
                kind,
                sender,
            } => {
                if let Some(reason) = rejected(settings, repo, sender, Some(body)) {
                    return Handled::Ignored(reason);
                }
                if !is_review_request(repo.platform(), body) {
                    return Handled::Ignored("not a review command".to_owned());
                }
                let target = ReviewTarget {
                    repo: repo.clone(),
                    number: *number,
                };
                let head = match self.head_of(&target).await {
                    Ok(head) => head,
                    Err(outcome) => return outcome,
                };
                let request = ReviewRequest {
                    target,
                    commit: Some(head.clone()),
                    trigger: "review command".to_owned(),
                    requester: Some(sender.login.clone()),
                    acknowledge: Some((
                        comment_id.clone(),
                        matches!(kind, CommentKind::Review { .. }),
                    )),
                    run: None,
                    submitted_at: None,
                    queued_check: None,
                };
                self.submit(request, head)
            }
            EventKind::ReviewRequested {
                target,
                commit,
                requester,
            } => {
                if !settings.allowlist.allows(&target.repo) {
                    return Handled::Ignored(format!("{} is not on the allowlist", target.repo));
                }
                let head = match commit {
                    Some(commit) => commit.clone(),
                    None => match self.head_of(target).await {
                        Ok(head) => head,
                        Err(outcome) => return outcome,
                    },
                };
                let request = ReviewRequest {
                    target: target.clone(),
                    commit: Some(head.clone()),
                    trigger: "requested".to_owned(),
                    requester: requester.clone(),
                    acknowledge: None,
                    run: None,
                    submitted_at: None,
                    queued_check: None,
                };
                self.submit(request, head)
            }
            EventKind::PlanRequested { .. }
            | EventKind::AddressRequested { .. }
            | EventKind::Unmodelled { .. }
            | EventKind::Ignored(_) => Handled::Ignored("not a review event".to_owned()),
        }
    }
}
