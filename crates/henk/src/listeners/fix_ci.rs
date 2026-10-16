//! Starts fix-CI runs (§3.7, #583): the `@meneer-henk fix ci` command on a
//! pull request, and direct requests from the API, the dashboard and MCP.

use std::sync::Arc;

use henk_domain::queue::Decision;
use henk_domain::review::is_fix_ci_request;
use henk_events::{CommentKind, Event, EventKind, Handled, Listener};
use henk_platform::ReviewTarget;
use tracing::{instrument, warn};

use super::Writers;
use super::filter::rejected;
use crate::coordinator::Coordinator;

/// Turns fix-CI commands and `FixCiRequested` into background fix-CI runs,
/// each followed by a review of the head it pushes.
pub struct FixCiListener {
    coordinator: Arc<Coordinator>,
    writers: Arc<dyn Writers>,
}

impl FixCiListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>, writers: Arc<dyn Writers>) -> Self {
        Self {
            coordinator,
            writers,
        }
    }
}

#[async_trait::async_trait]
impl Listener for FixCiListener {
    fn name(&self) -> &'static str {
        "fix_ci"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let settings = &self.coordinator.app().settings;
        let (target, trigger, requester) = match &event.kind {
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
                if !is_fix_ci_request(repo.platform(), body) {
                    return Handled::Ignored("not a fix ci command".to_owned());
                }
                let target = ReviewTarget {
                    repo: repo.clone(),
                    number: *number,
                };
                // Seen: the run's own comment says how it went.
                if let Ok(writer) = self.writers.writer(repo)
                    && let Err(error) = writer
                        .acknowledge(
                            &target,
                            comment_id,
                            matches!(kind, CommentKind::Review { .. }),
                        )
                        .await
                {
                    warn!(%error, "could not acknowledge the fix ci command");
                }
                (
                    target,
                    "fix ci command".to_owned(),
                    Some(sender.login.clone()),
                )
            }
            EventKind::FixCiRequested { target, requester } => {
                if !settings.allowlist.allows(&target.repo) {
                    return Handled::Ignored(format!("{} is not on the allowlist", target.repo));
                }
                let trigger = requester
                    .as_deref()
                    .map_or_else(|| "requested".to_owned(), |r| format!("requested by {r}"));
                (target.clone(), trigger, requester.clone())
            }
            _ => return Handled::Ignored("not a fix ci request".to_owned()),
        };
        if settings.roles.executor.is_none() {
            return Handled::Ignored("fixing checks needs [roles.executor]".to_owned());
        }
        match self.coordinator.submit_fix_ci(target, trigger, requester) {
            (Decision::Join, run) => Handled::Joined(run),
            (Decision::Supersede, run) => Handled::Superseded(run),
            (Decision::Start | Decision::After, run) => Handled::Started(run),
        }
    }
}
