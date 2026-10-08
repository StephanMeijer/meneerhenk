//! Runs reviews and plans in the background, one review per pull/merge
//! request at a time (§5.2; open question 2 decided in `henk_domain::queue`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use henk_domain::allowlist::Platform;
use henk_domain::queue::{Decision, decide};
use henk_domain::review::CommitSha;
use henk_domain::run::RunId;
use henk_platform::{IssueTarget, ReviewTarget};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::address::{AddressRequest, run_address};
use crate::app::App;
use crate::ids::new_run_id;
use crate::plan::{PlanRequest, run_plan};
use crate::review::{ReviewRequest, report_cancelled_while_queued, run_review};

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
    /// When the coordinator took it.
    since: time::OffsetDateTime,
    /// Whether it holds a review slot yet (#225).
    started: Arc<std::sync::atomic::AtomicBool>,
}

/// The review slots: how many there are, how many are taken, and what
/// waits for one (#225).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slots {
    /// `review.max_concurrent`.
    pub limit: usize,
    /// Slots a review holds now.
    pub in_use: usize,
    /// Reviews waiting for a slot, longest waiting first.
    pub waiting: Vec<Waiting>,
}

/// A review waiting for a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// `owner/name`.
    pub repo: String,
    /// The pull or merge request.
    pub number: u64,
    /// When the coordinator took it.
    pub since: time::OffsetDateTime,
}

/// A review of `generation` leaves the queue, unless a newer one took its
/// place; the overview hears that its slot is free.
fn leave(
    active: &Mutex<HashMap<Key, Active>>,
    key: &Key,
    generation: u64,
    slots_changed: &watch::Sender<()>,
) {
    if let Ok(mut map) = active.lock()
        && map.get(key).is_some_and(|a| a.generation == generation)
    {
        map.remove(key);
    }
    slots_changed.send_replace(());
}

/// Starts and tracks background runs.
pub struct Coordinator {
    app: Arc<App>,
    active: Arc<Mutex<HashMap<Key, Active>>>,
    review_slots: Arc<Semaphore>,
    generation: Mutex<u64>,
    addressing: Arc<Mutex<std::collections::HashSet<Key>>>,
    /// Marked whenever [`Coordinator::slots`] may read differently, so the
    /// overview's stream need not wait for its next snapshot.
    slots_changed: Arc<watch::Sender<()>>,
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
            slots_changed: Arc::new(watch::Sender::new(())),
        }
    }

    /// The application.
    #[must_use]
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// Reviews running or waiting for a slot right now.
    #[must_use]
    pub fn active_reviews(&self) -> usize {
        self.active.lock().map_or(0, |m| m.len())
    }

    /// The review slots and what waits for one.
    #[must_use]
    pub fn slots(&self) -> Slots {
        let limit = self.app.settings.review.max_concurrent.max(1);
        let in_use = limit.saturating_sub(self.review_slots.available_permits());
        let mut waiting: Vec<Waiting> = self.active.lock().map_or_else(
            |_| Vec::new(),
            |active| {
                active
                    .iter()
                    .filter(|(_, a)| !a.started.load(std::sync::atomic::Ordering::Relaxed))
                    .map(|(key, a)| Waiting {
                        repo: key.repo.clone(),
                        number: key.number,
                        since: a.since,
                    })
                    .collect()
            },
        );
        waiting.sort_by_key(|w| w.since);
        Slots {
            limit,
            in_use,
            waiting,
        }
    }

    /// Wakes when a review starts waiting, takes or frees a slot, or leaves
    /// the queue; [`Coordinator::slots`] then says how things stand.
    #[must_use]
    pub fn watch_slots(&self) -> watch::Receiver<()> {
        self.slots_changed.subscribe()
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
        let (decision, cancel, started) = {
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
                        // The old run learns which run replaced it (#231).
                        if !self.app.cancels.supersede(&old.run, &run) {
                            old.cancel.cancel();
                        }
                    }
                }
                Decision::Start => {}
            }
            // A child of the shutdown token: a shutdown reaches it too.
            let cancel = self.app.shutdown.child_token();
            let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
            active.insert(
                key.clone(),
                Active {
                    run: run.clone(),
                    commit: commit.clone(),
                    cancel: cancel.clone(),
                    generation,
                    since: time::OffsetDateTime::now_utc(),
                    started: Arc::clone(&started),
                },
            );
            (decision, cancel, started)
        };
        self.slots_changed.send_replace(());

        let app = Arc::clone(&self.app);
        let slots = Arc::clone(&self.review_slots);
        let active = Arc::clone(&self.active);
        let slots_changed = Arc::clone(&self.slots_changed);
        let request = ReviewRequest {
            commit: Some(commit),
            run: Some(run.clone()),
            submitted_at: Some(time::OffsetDateTime::now_utc()),
            ..request
        };
        let cancellable = self.app.cancels.register(run.clone(), cancel.clone());
        tokio::spawn(async move {
            let _cancellable = cancellable;
            // A review waiting for a slot still hears its token: a cancel
            // ends it now, not once a slot frees.
            let permit = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                permit = slots.acquire_owned() => match permit {
                    Ok(permit) => Some(permit),
                    Err(_) => return,
                },
            };
            started.store(true, std::sync::atomic::Ordering::Relaxed);
            slots_changed.send_replace(());
            if cancel.is_cancelled() {
                let by = request
                    .run
                    .as_ref()
                    .and_then(|run| app.cancels.cancelled_by(run));
                if let Some(by) = by {
                    if let Err(error) = report_cancelled_while_queued(&app, &request, &by).await {
                        warn!(%error, "could not record the cancelled review");
                    }
                } else {
                    info!("review superseded before it started");
                }
            } else if let Err(error) = run_review(&app, request, cancel).await {
                warn!(%error, "review ended with an error");
            }
            drop(permit);
            leave(&active, &key, generation, &slots_changed);
        });
        (decision, run)
    }

    /// Cancels a running review, plan or address run on behalf of `by`
    /// (#69). A cancelled review leaves the running set at once, so a new
    /// request for the same pull request starts afresh instead of joining
    /// it. False when the run is not one this process is running.
    pub fn cancel(&self, run: &RunId, by: String) -> bool {
        if !self.app.cancels.cancel(run, by) {
            return false;
        }
        if let Ok(mut active) = self.active.lock() {
            active.retain(|_, a| a.run != *run);
        }
        self.slots_changed.send_replace(());
        true
    }

    /// Starts a plan in the background and returns its run id.
    /// `requester` is who asked, for the run record, when known.
    pub fn submit_plan(
        &self,
        target: IssueTarget,
        note: Option<String>,
        trigger: String,
        requester: Option<String>,
    ) -> RunId {
        let run = new_run_id();
        let request = PlanRequest {
            target,
            note,
            trigger,
            run: Some(run.clone()),
            requester,
        };
        let app = Arc::clone(&self.app);
        let cancel = app.shutdown.child_token();
        let cancellable = app.cancels.register(run.clone(), cancel.clone());
        tokio::spawn(async move {
            let _cancellable = cancellable;
            if let Err(error) = run_plan(&app, request, cancel).await {
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
        requester: Option<String>,
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
            requester,
        };
        let app = Arc::clone(&self.app);
        let addressing = Arc::clone(&self.addressing);
        let cancel = app.shutdown.child_token();
        let cancellable = app.cancels.register(run.clone(), cancel.clone());
        tokio::spawn(async move {
            let _cancellable = cancellable;
            if let Err(error) = run_address(&app, request, cancel).await {
                warn!(%error, "address run ended with an error");
            }
            if let Ok(mut busy) = addressing.lock() {
                busy.remove(&key);
            }
        });
        Ok(run)
    }
}
