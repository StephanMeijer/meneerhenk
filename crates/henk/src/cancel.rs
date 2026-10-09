//! Cancelling one run (#69). While a review, plan or address run may run,
//! its cancellation token is registered here under its run id. Someone
//! signed in to the dashboard cancels it by id; the run then sees who did,
//! ends as `Cancelled` and says so in one comment, instead of reading the
//! cancellation as a supersede, a shutdown or its own failure. A review
//! superseded by a review of a newer commit learns here which run replaced
//! it (#231).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use henk_domain::run::RunId;
use tokio_util::sync::CancellationToken;

/// The runs that can be cancelled, and who cancelled which.
#[derive(Debug, Default, Clone)]
pub struct Cancels(Arc<Mutex<HashMap<RunId, Entry>>>);

#[derive(Debug)]
struct Entry {
    token: CancellationToken,
    by: Option<String>,
    superseded_by: Option<RunId>,
}

impl Cancels {
    /// Makes `run` cancellable through `token` until the guard is dropped.
    #[must_use]
    pub fn register(&self, run: RunId, token: CancellationToken) -> Registered {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                run.clone(),
                Entry {
                    token,
                    by: None,
                    superseded_by: None,
                },
            );
        Registered {
            cancels: self.clone(),
            run,
        }
    }

    /// Cancels `run` on behalf of `by`, a stable id such as `github:1234`.
    /// False when the run is not one this process can cancel: it ended, or
    /// it runs elsewhere.
    pub fn cancel(&self, run: &RunId, by: String) -> bool {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = runs.get_mut(run) else {
            return false;
        };
        // Who cancelled is set before the token fires, so the run sees it.
        entry.by.get_or_insert(by);
        entry.token.cancel();
        true
    }

    /// Cancels `run` because `by`, a review of a newer commit, replaces it.
    /// False when `run` is not registered here.
    pub fn supersede(&self, run: &RunId, by: &RunId) -> bool {
        let mut runs = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = runs.get_mut(run) else {
            return false;
        };
        // Set before the token fires, so the run sees it.
        entry.superseded_by.get_or_insert_with(|| by.clone());
        entry.token.cancel();
        true
    }

    /// The run that replaced `run`, when it was superseded.
    #[must_use]
    pub fn superseded_by(&self, run: &RunId) -> Option<RunId> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .and_then(|entry| entry.superseded_by.clone())
    }

    /// Who cancelled `run`, when a person did.
    #[must_use]
    pub fn cancelled_by(&self, run: &RunId) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .and_then(|entry| entry.by.clone())
    }
}

/// The error of a run that stopped because its cancellation token fired.
/// Only this error ends a run as cancelled by a person or an MCP client: a run that
/// failed for another reason after someone asked for a cancel is reported
/// as the failure it is.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

/// Whether `failure` is a run stopping for its cancellation token.
#[must_use]
pub fn is_cancelled(failure: &anyhow::Error) -> bool {
    failure.downcast_ref::<Cancelled>().is_some()
}

/// Keeps a run in [`Cancels`] until dropped.
#[derive(Debug)]
pub struct Registered {
    cancels: Cancels,
    run: RunId,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.cancels
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.run);
    }
}

/// Where a cancel by `by` came from, by its id's prefix: `github:` the
/// dashboard, `mcp:` an MCP client, anything else unsaid. The one rule the
/// notice, the run record and the log all follow (#293).
#[must_use]
pub fn cancelled_via(by: &str) -> &'static str {
    if by.starts_with("github:") {
        "from the dashboard"
    } else if by.starts_with("mcp:") {
        "over MCP"
    } else {
        ""
    }
}

/// Why a cancelled run ended, as the run records it: where the cancel came
/// from and the canceller's id (§2), `cancelled over MCP by mcp:claude`.
#[must_use]
pub fn cancelled_reason(by: &str) -> String {
    match cancelled_via(by) {
        "" => format!("cancelled by {by}"),
        via => format!("cancelled {via} by {by}"),
    }
}

/// The comment a cancelled run leaves, without its marker. `by` is the
/// requester id, never a display name (§2); [`cancelled_via`] says where the
/// cancel came from, and the account is named as a reader knows it.
/// `nothing_pushed` adds that nothing was pushed, for an address run, the
/// one run that pushes.
#[must_use]
pub fn cancelled_notice(by: &str, link: &str, nothing_pushed: bool) -> String {
    let who = if let Some(id) = by.strip_prefix("github:") {
        format!("GitHub account {id}")
    } else if let Some(name) = by.strip_prefix("mcp:") {
        format!("client {name}")
    } else {
        by.to_owned()
    };
    let how = match cancelled_via(by) {
        "" => format!("by {who}"),
        via => format!("{via} by {who}"),
    };
    let pushed = if nothing_pushed {
        " Nothing was pushed."
    } else {
        ""
    };
    format!("Cancelled {how}.{pushed}\n\nRun: {link}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_run_can_be_cancelled_while_registered_and_says_by_whom() {
        let cancels = Cancels::default();
        let run = RunId::parse("r-1").unwrap();
        let token = CancellationToken::new();
        let registered = cancels.register(run.clone(), token.clone());
        assert_eq!(cancels.cancelled_by(&run), None);
        assert!(cancels.cancel(&run, "github:1234".to_owned()));
        assert!(token.is_cancelled());
        assert_eq!(cancels.cancelled_by(&run).as_deref(), Some("github:1234"));
        assert!(
            cancels.cancel(&run, "github:9".to_owned()),
            "a second cancel is harmless"
        );
        assert_eq!(
            cancels.cancelled_by(&run).as_deref(),
            Some("github:1234"),
            "the first one counts"
        );
        drop(registered);
        assert!(!cancels.cancel(&run, "github:1234".to_owned()), "ended");
        assert_eq!(cancels.cancelled_by(&run), None);
    }

    #[test]
    fn a_superseded_run_learns_which_run_replaced_it_and_no_person_did() {
        let cancels = Cancels::default();
        let (old, new) = (
            RunId::parse("r-old").unwrap(),
            RunId::parse("r-new").unwrap(),
        );
        let token = CancellationToken::new();
        let registered = cancels.register(old.clone(), token.clone());
        assert_eq!(cancels.superseded_by(&old), None);
        assert!(cancels.supersede(&old, &new));
        assert!(token.is_cancelled());
        assert_eq!(cancels.superseded_by(&old), Some(new.clone()));
        assert_eq!(
            cancels.cancelled_by(&old),
            None,
            "a supersede is not a person's cancel"
        );
        drop(registered);
        assert!(!cancels.supersede(&old, &new), "ended");
    }

    #[test]
    fn the_notice_names_an_id_and_is_in_style() {
        let review = cancelled_notice("github:1234", "https://henk/runs/r-1", false);
        assert_eq!(
            review,
            "Cancelled from the dashboard by GitHub account 1234.\n\nRun: https://henk/runs/r-1"
        );
        let address = cancelled_notice("github:1234", "https://henk/runs/r-1", true);
        assert!(address.contains("Nothing was pushed."));
        let mcp = cancelled_notice("mcp:claude", "https://henk/runs/r-1", false);
        assert_eq!(
            mcp, "Cancelled over MCP by client claude.\n\nRun: https://henk/runs/r-1",
            "not from the dashboard"
        );
        let other = cancelled_notice("discord:3", "https://henk/runs/r-1", false);
        assert_eq!(
            other,
            "Cancelled by discord:3.\n\nRun: https://henk/runs/r-1"
        );
        for text in [review, address, mcp, other] {
            assert!(henk_domain::text::is_in_style(&text), "{text}");
        }
    }

    #[test]
    fn the_record_says_where_a_cancel_came_from_as_the_notice_does() {
        for (by, via, reason) in [
            (
                "github:1234",
                "from the dashboard",
                "cancelled from the dashboard by github:1234",
            ),
            ("mcp:claude", "over MCP", "cancelled over MCP by mcp:claude"),
            ("discord:3", "", "cancelled by discord:3"),
        ] {
            assert_eq!(cancelled_via(by), via, "{by}");
            assert_eq!(cancelled_reason(by), reason, "{by}");
            let notice = cancelled_notice(by, "https://henk/runs/r-1", false);
            assert!(notice.contains(via), "{notice}");
        }
    }
}
