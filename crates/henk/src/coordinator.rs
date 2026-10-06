//! Runs reviews and plans in the background, one review per pull/merge
//! request at a time (§5.2; open question 2 decided in `henk_domain::queue`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use henk_domain::allowlist::Platform;
use henk_domain::queue::{Decision, decide};
use henk_domain::review::CommitSha;
use henk_domain::run::RunId;
use henk_platform::{IssueTarget, ReviewTarget};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::address::{AddressRequest, run_address};
use crate::app::App;
use crate::ids::new_run_id;
use crate::plan::{PlanRequest, run_plan};
use crate::review::{ReviewRequest, run_review};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    platform: Platform,
    repo: String,
    number: u64,
}

impl Key {
    fn of(target: &ReviewTarget) -> Self {
        Self {
            platform: target.platform(),
            repo: target.repo.path(),
            number: target.number,
        }
    }
}

#[derive(Debug)]
struct Active {
    run: RunId,
    commit: CommitSha,
    cancel: CancellationToken,
    generation: u64,
}

/// Starts and tracks background runs.
pub struct Coordinator {
    app: Arc<App>,
    active: Arc<Mutex<HashMap<Key, Active>>>,
    review_slots: Arc<Semaphore>,
    generation: Mutex<u64>,
    addressing: Arc<Mutex<std::collections::HashSet<Key>>>,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator").finish_non_exhaustive()
    }
}

impl Coordinator {
    /// Builds a coordinator over the shared application.
    #[must_use]
    pub fn new(app: Arc<App>) -> Self {
        let slots = app.settings.review.max_concurrent.max(1);
        Self {
            app,
            active: Arc::new(Mutex::new(HashMap::new())),
            review_slots: Arc::new(Semaphore::new(slots)),
            generation: Mutex::new(0),
            addressing: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// The application.
    #[must_use]
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// Reviews running right now.
    #[must_use]
    pub fn active_reviews(&self) -> usize {
        self.active.lock().map_or(0, |m| m.len())
    }

    /// Decides what to do with a review request for `commit` and acts on it.
    /// Returns the decision and the run it concerns: the new run, or the one joined.
    pub fn submit_review(&self, request: ReviewRequest, commit: CommitSha) -> (Decision, RunId) {
        let key = Key::of(&request.target);
        let generation = {
            let Ok(mut counter) = self.generation.lock() else {
                return (Decision::Start, new_run_id());
            };
            *counter += 1;
            *counter
        };
        let run = new_run_id();
        let (decision, cancel) = {
            let Ok(mut active) = self.active.lock() else {
                error!("coordinator lock poisoned");
                return (Decision::Start, run);
            };
            let decision = decide(active.get(&key).map(|a| &a.commit), &commit);
            match decision {
                Decision::Join => {
                    let joined = active
                        .get(&key)
                        .map_or_else(|| run.clone(), |a| a.run.clone());
                    info!(repo = %key.repo, number = key.number, run = %joined, "joined the running review");
                    // Recorded off the lock: the store is async and this guard is not.
                    let store = Arc::clone(&self.app.store);
                    let (run_id, trigger) = (joined.clone(), request.trigger.clone());
                    tokio::spawn(async move {
                        let _ = store.joined(&run_id, &trigger).await;
                    });
                    return (Decision::Join, joined);
                }
                Decision::Supersede => {
                    if let Some(old) = active.remove(&key) {
                        warn!(repo = %key.repo, number = key.number, old = %old.commit.short(), new = %commit.short(), "superseding a running review");
                        old.cancel.cancel();
                    }
                }
                Decision::Start => {}
            }
            // A child of the shutdown token: a shutdown reaches it too.
            let cancel = self.app.shutdown.child_token();
            active.insert(
                key.clone(),
                Active {
                    run: run.clone(),
                    commit: commit.clone(),
                    cancel: cancel.clone(),
                    generation,
                },
            );
            (decision, cancel)
        };

        let app = Arc::clone(&self.app);
        let slots = Arc::clone(&self.review_slots);
        let active = Arc::clone(&self.active);
        let request = ReviewRequest {
            commit: Some(commit),
            run: Some(run.clone()),
            ..request
        };
        tokio::spawn(async move {
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            if cancel.is_cancelled() {
                info!("review superseded before it started");
            } else if let Err(error) = run_review(&app, request, cancel).await {
                warn!(%error, "review ended with an error");
            }
            if let Ok(mut map) = active.lock()
                && map.get(&key).is_some_and(|a| a.generation == generation)
            {
                map.remove(&key);
            }
        });
        (decision, run)
    }

    /// Starts a plan in the background and returns its run id.
    pub fn submit_plan(&self, target: IssueTarget, note: Option<String>, trigger: String) -> RunId {
        let run = new_run_id();
        let request = PlanRequest {
            target,
            note,
            trigger,
            run: Some(run.clone()),
        };
        let app = Arc::clone(&self.app);
        tokio::spawn(async move {
            if let Err(error) = run_plan(&app, request, app.shutdown.child_token()).await {
                warn!(%error, "plan ended with an error");
            }
        });
        run
    }

    /// Starts an address run (§3.5) in the background. One per pull request
    /// at a time: a second request while one runs is refused, not queued, so
    /// two runs never push to the same branch.
    ///
    /// # Errors
    ///
    /// Returns the reason when an address run is already going on it.
    pub fn submit_address(
        &self,
        target: ReviewTarget,
        note: Option<String>,
        trigger: String,
    ) -> Result<RunId, String> {
        let key = Key::of(&target);
        {
            let Ok(mut busy) = self.addressing.lock() else {
                return Err("coordinator lock poisoned".to_owned());
            };
            if !busy.insert(key.clone()) {
                return Err(format!(
                    "an address run is already going on #{}",
                    target.number
                ));
            }
        }
        let run = new_run_id();
        let request = AddressRequest {
            target,
            note,
            trigger,
            run: Some(run.clone()),
        };
        let app = Arc::clone(&self.app);
        let addressing = Arc::clone(&self.addressing);
        tokio::spawn(async move {
            if let Err(error) = run_address(&app, request, app.shutdown.child_token()).await {
                warn!(%error, "address run ended with an error");
            }
            if let Ok(mut busy) = addressing.lock() {
                busy.remove(&key);
            }
        });
        Ok(run)
    }
}
