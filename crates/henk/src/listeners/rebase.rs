//! Starts rebase runs (§3.6, #579): the `@meneer-henk rebase` command on a
//! pull request, and direct requests from the API, the dashboard and MCP.

use std::sync::Arc;

use henk_domain::queue::Decision;
use henk_domain::review::is_rebase_request;
use henk_events::{CommentKind, Event, EventKind, Handled, Listener};
use henk_platform::ReviewTarget;
use tracing::{instrument, warn};

use super::Writers;
use super::filter::rejected;
use crate::coordinator::Coordinator;

/// Turns rebase commands and `RebaseRequested` into background rebase
/// runs, each followed by a review of the head it pushes.
pub struct RebaseListener {
    coordinator: Arc<Coordinator>,
    writers: Arc<dyn Writers>,
}

impl RebaseListener {
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
impl Listener for RebaseListener {
    fn name(&self) -> &'static str {
        "rebase"
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
                if !is_rebase_request(repo.platform(), body) {
                    return Handled::Ignored("not a rebase command".to_owned());
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
                    warn!(%error, "could not acknowledge the rebase command");
                }
                (
                    target,
                    "rebase command".to_owned(),
                    Some(sender.login.clone()),
                )
            }
            EventKind::RebaseRequested { target, requester } => {
                if !settings.allowlist.allows(&target.repo) {
                    return Handled::Ignored(format!("{} is not on the allowlist", target.repo));
                }
                let trigger = requester
                    .as_deref()
                    .map_or_else(|| "requested".to_owned(), |r| format!("requested by {r}"));
                (target.clone(), trigger, requester.clone())
            }
            _ => return Handled::Ignored("not a rebase request".to_owned()),
        };
        if settings.roles.executor.is_none() {
            return Handled::Ignored("rebasing needs [roles.executor]".to_owned());
        }
        match self.coordinator.submit_rebase(target, trigger, requester) {
            (Decision::Join, run) => Handled::Joined(run),
            (Decision::Supersede, run) => Handled::Superseded(run),
            (Decision::Start | Decision::After, run) => Handled::Started(run),
        }
    }
}
