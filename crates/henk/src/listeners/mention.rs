//! Greets mentions (§3.4). No model is involved, so a mention cannot inject anything.

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::review::{is_review_request, mentions_henk};
use henk_events::{CommentKind, Event, EventKind, Handled, Listener};
use henk_platform::ReviewTarget;
use tracing::{info, instrument, warn};

use super::Writers;
use super::filter::rejected;
use crate::config::Settings;
use crate::ids::new_run_id;

/// Replies to comments that mention Henk and are not review commands.
pub struct MentionListener {
    settings: Arc<Settings>,
    writers: Arc<dyn Writers>,
}

impl MentionListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(settings: Arc<Settings>, writers: Arc<dyn Writers>) -> Self {
        Self { settings, writers }
    }
}

#[async_trait::async_trait]
impl Listener for MentionListener {
    fn name(&self) -> &'static str {
        "mention"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let EventKind::Comment {
            repo,
            number,
            body,
            comment_id,
            kind,
            sender,
        } = &event.kind
        else {
            return Handled::Ignored("not a comment".to_owned());
        };
        if let Some(reason) = rejected(&self.settings, repo, sender, Some(body)) {
            return Handled::Ignored(reason);
        }
        let platform = repo.platform();
        // A review command is a command, not a mention: the review listener takes it.
        if is_review_request(platform, body) {
            return Handled::Ignored("a review command, not a mention".to_owned());
        }
        if !mentions_henk(platform, body) {
            return Handled::Ignored("does not mention Henk".to_owned());
        }
        let writer = match self.writers.writer(platform) {
            Ok(writer) => writer,
            Err(error) => return Handled::Ignored(error.to_string()),
        };
        let target = ReviewTarget {
            repo: repo.clone(),
            number: *number,
        };
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
        let reply_body = Marker {
            run: new_run_id(),
            model: ModelId::parse("none").unwrap_or_else(|_| unreachable!("constant")),
            requested_by: None,
            kind: Some(MarkerKind::Reply),
            checked_by: None,
            withdrawn: None,
        }
        .attach(&text);
        // On GitLab a diff note reply goes to its discussion id, which the
        // parser puts in `in_reply_to`.
        let (reply_to, is_review_comment) = match (platform, kind) {
            (
                Platform::GitLab,
                CommentKind::Review {
                    in_reply_to: Some(discussion),
                },
            ) => (discussion.as_str(), true),
            (_, CommentKind::Review { .. }) => (comment_id.as_str(), true),
            (_, CommentKind::Conversation) => (comment_id.as_str(), false),
        };
        match writer
            .reply(&target, reply_to, is_review_comment, &reply_body)
            .await
        {
            Ok(_) => {
                info!(repo = %target.repo.path(), number = target.number, "greeted a mention");
                Handled::Greeted
            }
            Err(error) => {
                warn!(%error, "could not post a greeting");
                Handled::Failed(format!("greeting failed: {error}"))
            }
        }
    }
}
