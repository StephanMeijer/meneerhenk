//! Cancelling one run (#69). While a review, plan or address run may run,
//! its cancellation token is registered here under its run id. Someone
//! signed in to the dashboard cancels it by id; the run then sees who did,
//! ends as `Cancelled` and says so in one comment, instead of reading the
//! cancellation as a supersede, a shutdown or its own failure.

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
}

impl Cancels {
    /// Makes `run` cancellable through `token` until the guard is dropped.
    #[must_use]
    pub fn register(&self, run: RunId, token: CancellationToken) -> Registered {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run.clone(), Entry { token, by: None });
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
/// Only this error ends a run as cancelled from the dashboard: a run that
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

/// The comment a run cancelled from the dashboard leaves, without its
/// marker. `by` is an id, never a display name (§2). `nothing_pushed` adds
/// that nothing was pushed, for an address run, the one run that pushes.
#[must_use]
pub fn cancelled_notice(by: &str, link: &str, nothing_pushed: bool) -> String {
    let who = by
        .strip_prefix("github:")
        .map_or_else(|| by.to_owned(), |id| format!("GitHub account {id}"));
    let pushed = if nothing_pushed {
        " Nothing was pushed."
    } else {
        ""
    };
    format!("Cancelled from the dashboard by {who}.{pushed}\n\nRun: {link}")
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
    fn the_notice_names_an_id_and_is_in_style() {
        let review = cancelled_notice("github:1234", "https://henk/runs/r-1", false);
        assert_eq!(
            review,
            "Cancelled from the dashboard by GitHub account 1234.\n\nRun: https://henk/runs/r-1"
        );
        let address = cancelled_notice("github:1234", "https://henk/runs/r-1", true);
        assert!(address.contains("Nothing was pushed."));
        for text in [review, address] {
            assert!(henk_domain::text::is_in_style(&text), "{text}");
        }
    }
}
