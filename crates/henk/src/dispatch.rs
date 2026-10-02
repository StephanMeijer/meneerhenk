//! Turns platform events into work: reviews, greetings, or nothing (§3.1,
//! §3.4, §8.1).

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::queue::Decision;
use henk_domain::review::{is_review_request, mentions_henk};
use henk_platform::{
    CommentKind, IncomingEvent, PlatformWriter, PullRequestAction, ReviewTarget, Sender,
};
use tracing::{info, instrument, warn};

use crate::coordinator::Coordinator;
use crate::ids::new_run_id;
use crate::review::ReviewRequest;

/// What the dispatcher did with an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatched {
    /// Nothing, for this reason.
    Ignored(String),
    /// A review was started, joined or superseded another.
    Review(Decision),
    /// A greeting was posted.
    Greeted,
}

/// The dispatcher.
pub struct Dispatcher {
    coordinator: Arc<Coordinator>,
}

impl Dispatcher {
    /// Builds a dispatcher.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }

    fn is_henk(&self, platform: Platform, sender: &Sender) -> bool {
        let settings = &self.coordinator.app().settings;
        let own_login = match platform {
            Platform::GitHub => settings.github.as_ref().map(|g| g.bot_login.as_str()),
            Platform::GitLab => settings.gitlab.as_ref().map(|g| g.username.as_str()),
        };
        own_login.is_some_and(|login| login.eq_ignore_ascii_case(&sender.login))
    }

    /// Handles one event.
    #[instrument(skip_all)]
    pub async fn handle(&self, event: IncomingEvent) -> Dispatched {
        let app = self.coordinator.app();
        match event {
            IncomingEvent::Ignored(reason) => Dispatched::Ignored(reason),
            IncomingEvent::PullRequest {
                repo,
                number,
                action,
                head,
                draft,
                sender,
            } => {
                let platform = repo.platform();
                if sender.is_bot || self.is_henk(platform, &sender) {
                    return Dispatched::Ignored("sent by a bot".to_owned());
                }
                if !app.settings.allowlist.allows(&repo) {
                    return Dispatched::Ignored(format!("{repo} is not on the allowlist"));
                }
                // GitLab drafts are never reviewed (§3.1); GitHub drafts only when configured.
                let drafts_reviewed =
                    platform == Platform::GitHub && app.settings.review.github_drafts;
                if draft && !drafts_reviewed {
                    return Dispatched::Ignored(
                        "draft; not reviewed until marked ready".to_owned(),
                    );
                }
                let trigger = match action {
                    PullRequestAction::Opened => "opened",
                    PullRequestAction::Reopened => "reopened",
                    PullRequestAction::Synchronized => "new commits",
                };
                let target = ReviewTarget { repo, number };
                let request = ReviewRequest {
                    target,
                    commit: Some(head.clone()),
                    trigger: trigger.to_owned(),
                    requester: Some(sender.login),
                    acknowledge: None,
                };
                Dispatched::Review(self.coordinator.submit_review(request, head))
            }
            IncomingEvent::Comment {
                repo,
                number,
                body,
                comment_id,
                kind,
                sender,
            } => {
                let platform = repo.platform();
                if sender.is_bot || self.is_henk(platform, &sender) || Marker::is_present(&body) {
                    return Dispatched::Ignored("Henk's own words or a bot".to_owned());
                }
                if !app.settings.allowlist.allows(&repo) {
                    return Dispatched::Ignored(format!("{repo} is not on the allowlist"));
                }
                let target = ReviewTarget { repo, number };
                let is_review_comment = matches!(kind, CommentKind::Review { .. });
                if is_review_request(platform, &body) {
                    let writer = match app.writer(platform) {
                        Ok(writer) => writer,
                        Err(error) => return Dispatched::Ignored(error.to_string()),
                    };
                    let head = match writer.pull_request(&target).await {
                        Ok(info) if info.state == henk_platform::PullRequestState::Open => {
                            info.head
                        }
                        Ok(_) => return Dispatched::Ignored("pull request is not open".to_owned()),
                        Err(error) => {
                            return Dispatched::Ignored(format!(
                                "could not read the pull request: {error}"
                            ));
                        }
                    };
                    let request = ReviewRequest {
                        target,
                        commit: Some(head.clone()),
                        trigger: "review command".to_owned(),
                        requester: Some(sender.login),
                        acknowledge: Some((comment_id, is_review_comment)),
                    };
                    return Dispatched::Review(self.coordinator.submit_review(request, head));
                }
                if mentions_henk(platform, &body) {
                    let writer = match app.writer(platform) {
                        Ok(writer) => writer,
                        Err(error) => return Dispatched::Ignored(error.to_string()),
                    };
                    return greet(&writer, &target, &comment_id, &kind, platform).await;
                }
                Dispatched::Ignored("a comment that is neither a command nor a mention".to_owned())
            }
        }
    }
}

/// Posts a fixed greeting in reply to a mention (§3.4). No model is involved.
async fn greet(
    writer: &Arc<dyn PlatformWriter>,
    target: &ReviewTarget,
    comment_id: &str,
    kind: &CommentKind,
    platform: Platform,
) -> Dispatched {
    let key = comment_id.bytes().fold(0_u64, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u64::from(b))
    });
    let text = henk_agent::prompts::greeting(key);
    let text = match platform {
        Platform::GitHub => text.to_owned(),
        Platform::GitLab => text
            .replace("@meneer-henk", "@meneerhenk")
            .replace("pull request", "merge request"),
    };
    let body = Marker {
        run: new_run_id(),
        model: ModelId::parse("none").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Reply),
    }
    .attach(&text);
    // On GitLab a diff note reply goes to its discussion id, which events.rs
    // puts in `in_reply_to`.
    let (reply_to, is_review_comment) = match (platform, kind) {
        (
            Platform::GitLab,
            CommentKind::Review {
                in_reply_to: Some(discussion),
            },
        ) => (discussion.as_str(), true),
        (_, CommentKind::Review { .. }) => (comment_id, true),
        (_, CommentKind::Conversation) => (comment_id, false),
    };
    match writer
        .reply(target, reply_to, is_review_comment, &body)
        .await
    {
        Ok(_) => {
            info!(repo = %target.repo.path(), number = target.number, "greeted a mention");
            Dispatched::Greeted
        }
        Err(error) => {
            warn!(%error, "could not post a greeting");
            Dispatched::Ignored(format!("greeting failed: {error}"))
        }
    }
}
