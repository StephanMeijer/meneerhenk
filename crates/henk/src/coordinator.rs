//! Runs reviews and plans in the background, one review per pull/merge
//! request at a time (§5.2; open question 2 decided in `henk_domain::queue`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use henk_domain::allowlist::Platform;
use henk_domain::queue::{Decision, decide};
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_domain::run::RunId;
use henk_platform::{IssueTarget, ReviewHandle, ReviewTarget};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::address::{AddressRequest, run_address};
use crate::app::App;
use crate::ids::new_run_id;
use crate::plan::{PlanRequest, run_plan};
use crate::review::{
    ReviewRequest, report_cancelled_while_queued, report_interrupted_while_queued, run_review,
};

/// One pull or merge request, as the coordinator tells them apart. The
/// repository path is lowercase: neither platform's paths are
/// case-sensitive, and the allowlist compares them the same way (#57).
/// It is for telling them apart only: [`Active::repo`] keeps the path as
/// written, for the queue and the logs.
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
            repo: target.repo.path().to_ascii_lowercase(),
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
    /// The repository path as the request wrote it, which the run record
    /// keeps too; the key's is lowercase.
    repo: String,
    /// When the coordinator took it.
    since: time::OffsetDateTime,
    /// Whether it holds a review slot yet (#225).
    started: Arc<std::sync::atomic::AtomicBool>,
    /// What started it and who asked, for the queue (#251).
    trigger: String,
    requester: Option<String>,
}

/// The review slots: how many there are, how many are taken, and what
/// waits for one (#225).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slots {
    /// `review.max_concurrent`.
    pub limit: usize,
    /// Slots a review holds now.
    pub in_use: usize,
    /// Reviews waiting for a slot, in the order they will start.
    pub waiting: Vec<Waiting>,
}

/// Why a review waits (#251).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Every review slot is taken. A review never waits behind another of
    /// the same pull request: a new commit supersedes it, the same commit
    /// joins it.
    NoSlot,
}

impl Reason {
    /// The API's word for it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSlot => "no_slot",
        }
    }
}

/// A review waiting for a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// The run it will be; no record exists until it starts.
    pub run: RunId,
    /// Where the pull or merge request is.
    pub platform: Platform,
    /// `owner/name`.
    pub repo: String,
    /// The pull or merge request.
    pub number: u64,
    /// The commit it will review.
    pub commit: CommitSha,
    /// What started it.
    pub trigger: String,
    /// Who asked, as a stable id, when someone did.
    pub requester: Option<String>,
    /// When the coordinator took it.
    pub since: time::OffsetDateTime,
    /// Its place in the queue, from 1.
    pub position: usize,
    /// Why it waits.
    pub reason: Reason,
}

/// A waiting review as the queue shows it; its position is set by the caller.
fn waiting(key: &Key, active: &Active) -> Waiting {
    Waiting {
        run: active.run.clone(),
        platform: key.platform,
        repo: active.repo.clone(),
        number: key.number,
        commit: active.commit.clone(),
        trigger: active.trigger.clone(),
        requester: active.requester.clone(),
        since: active.since,
        position: 0,
        reason: Reason::NoSlot,
    }
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

/// Opens the review's check as queued on the pull request (#262), when
/// the platform has one and the review could run once it has a slot.
/// Henk's failure to open it is logged; the review waits all the same.
async fn queue_check(app: &App, request: &ReviewRequest) -> Option<ReviewHandle> {
    let (Some(run), Some(commit)) = (&request.run, &request.commit) else {
        return None;
    };
    if !app.settings.allowlist.allows(&request.target.repo) || app.settings.lanes.is_empty() {
        return None;
    }
    let writer = app.writer(request.target.platform()).ok()?;
    let limit = app.settings.review.max_concurrent.max(1);
    let summary = format!(
        "Waiting for a review slot: all {limit} are in use. Henk starts this review when one frees."
    );
    let link = app.settings.queue_link(run);
    match writer
        .queue_review(&request.target, commit, "Queued", &summary, &link)
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            warn!(%error, run = %run, "could not show the review as queued");
            None
        }
    }
}

/// Waits for a review slot while the pull request shows the review queued
/// (#262). It joins the slot queue before it asks the platform to show the
/// check queued, so a slow platform costs it no place: reviews start in the
/// order they came, as [`Coordinator::slots`] says. A cancel ends the wait
/// at once, not once a slot frees. Whatever ends it, the queued check is in
/// `request` before this returns, so it closes. `None` when cancelled.
async fn wait_for_slot(
    app: &App,
    slots: &Arc<Semaphore>,
    cancel: &CancellationToken,
    request: &mut ReviewRequest,
) -> Option<Result<OwnedSemaphorePermit, AcquireError>> {
    let (waited, queued_check) = {
        // Boxed so a cancelled review leaves the queue before the platform
        // answers.
        let mut acquire = Box::pin(Arc::clone(slots).acquire_owned());
        let mut queue = std::pin::pin!(queue_check(app, request));
        let mut queued_check = None;
        let mut asked = false;
        let waited = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break None,
                permit = &mut acquire => break Some(permit),
                check = &mut queue, if !asked => {
                    queued_check = check;
                    asked = true;
                }
            }
        };
        drop(acquire);
        if !asked {
            queued_check = queue.await;
        }
        (waited, queued_check)
    };
    request.queued_check = queued_check;
    waited
}

/// A review that never got its slot: cancelled from the dashboard or over MCP,
/// superseded by a newer commit, or stopped with Henk. Its queued check,
/// if it has one, is closed so nothing stays queued (#262). One stopped
/// with Henk is recorded as an interrupted run, so the next start resumes
/// it (#160).
async fn ended_while_queued(app: &App, request: &ReviewRequest) {
    let Some(run) = request.run.as_ref() else {
        return;
    };
    if let Some(by) = app.cancels.cancelled_by(run) {
        if let Err(error) = report_cancelled_while_queued(app, request, &by).await {
            warn!(%error, "could not record the cancelled review");
        }
        return;
    }
    if app.cancels.superseded_by(run).is_none() && app.shutdown.is_cancelled() {
        // Henk stops: the run is recorded as interrupted, so the next
        // start resumes it like a review that was running (#160).
        if let Err(error) = report_interrupted_while_queued(app, request).await {
            warn!(%error, "could not record the interrupted review");
        }
        return;
    }
    let (Some(handle), Some(commit)) = (&request.queued_check, &request.commit) else {
        info!("review stopped before it started");
        return;
    };
    let outcome = if app.cancels.superseded_by(run).is_some() {
        info!("review superseded before it started");
        ReviewOutcome::superseded(commit.clone())
    } else {
        info!("review stopped with Henk before it started");
        ReviewOutcome::interrupted(commit.clone())
    };
    let link = app.settings.queue_link(run);
    match app.writer(request.target.platform()) {
        Ok(writer) => {
            if let Err(error) = writer
                .finish_review(&request.target, commit, Some(handle), &outcome, &link)
                .await
            {
                warn!(%error, "could not close the queued check");
            }
        }
        Err(error) => warn!(%error, "could not close the queued check"),
    }
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

    /// Reviews running or waiting for a slot right now: what a shutdown
    /// waits for, as both hear its token.
    #[must_use]
    pub fn tracked_reviews(&self) -> usize {
        self.active.lock().map_or(0, |m| m.len())
    }

    /// Reviews that hold a slot right now (#251: a waiting one does not
    /// count as running).
    #[must_use]
    pub fn running_reviews(&self) -> usize {
        self.active.lock().map_or(0, |m| {
            m.values()
                .filter(|a| a.started.load(std::sync::atomic::Ordering::Relaxed))
                .count()
        })
    }

    /// Reviews waiting for a slot right now.
    #[must_use]
    pub fn queued_reviews(&self) -> usize {
        self.active.lock().map_or(0, |m| {
            m.values()
                .filter(|a| !a.started.load(std::sync::atomic::Ordering::Relaxed))
                .count()
        })
    }

    /// The review slots and what waits for one.
    #[must_use]
    pub fn slots(&self) -> Slots {
        let limit = self.app.settings.review.max_concurrent.max(1);
        let in_use = limit.saturating_sub(self.review_slots.available_permits());
        // In the order they were taken. The slot semaphore is fair, so that
        // is the order they start in, short of two requests within the same
        // scheduler tick asking for a slot the other way round.
        let mut queued: Vec<(u64, Waiting)> = self.active.lock().map_or_else(
            |_| Vec::new(),
            |active| {
                active
                    .iter()
                    .filter(|(_, a)| !a.started.load(std::sync::atomic::Ordering::Relaxed))
                    .map(|(key, a)| (a.generation, waiting(key, a)))
                    .collect()
            },
        );
        queued.sort_by_key(|(generation, _)| *generation);
        let waiting = queued
            .into_iter()
            .enumerate()
            .map(|(at, (_, entry))| Waiting {
                position: at + 1,
                ..entry
            })
            .collect();
        Slots {
            limit,
            in_use,
            waiting,
        }
    }

    /// Takes a review slot as a review would, so a test can fill them.
    #[cfg(test)]
    pub(crate) fn take_a_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.review_slots).try_acquire_owned().ok()
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
                    info!(repo = %request.target.repo.path(), number = key.number, run = %joined, "joined the running review");
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
                        warn!(repo = %request.target.repo.path(), number = key.number, old = %old.commit.short(), new = %commit.short(), "superseding a running review");
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
                    repo: request.target.repo.path(),
                    since: time::OffsetDateTime::now_utc(),
                    started: Arc::clone(&started),
                    trigger: request.trigger.clone(),
                    requester: request.requester.clone(),
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
            let mut request = request;
            // A free slot starts the review at once, as before. Otherwise
            // the pull request shows it queued while it waits (#262).
            let permit = match Arc::clone(&slots).try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(tokio::sync::TryAcquireError::Closed) => return,
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    let waited = wait_for_slot(&app, &slots, &cancel, &mut request).await;
                    match waited {
                        None => None,
                        Some(Ok(permit)) => Some(permit),
                        Some(Err(_)) => {
                            // The slots closed under it: its queued check
                            // still closes (#262).
                            ended_while_queued(&app, &request).await;
                            return;
                        }
                    }
                }
            };
            started.store(true, std::sync::atomic::Ordering::Relaxed);
            slots_changed.send_replace(());
            if cancel.is_cancelled() {
                ended_while_queued(&app, &request).await;
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use henk_domain::allowlist::RepoRef;

    use super::*;

    fn target(platform: Platform, path: &str, number: u64) -> ReviewTarget {
        ReviewTarget {
            repo: RepoRef::parse(platform, path).unwrap(),
            number,
        }
    }

    #[test]
    fn a_path_differing_only_in_case_is_the_same_pull_request() {
        let key = Key::of(&target(Platform::GitHub, "O/R", 7));
        assert_eq!(key, Key::of(&target(Platform::GitHub, "o/r", 7)));
        assert_ne!(
            key,
            Key::of(&target(Platform::GitHub, "O/S", 7)),
            "another repository"
        );
        assert_ne!(
            key,
            Key::of(&target(Platform::GitHub, "o/r", 8)),
            "another number"
        );
        assert_ne!(
            key,
            Key::of(&target(Platform::GitLab, "o/r", 7)),
            "another platform"
        );
    }

    #[test]
    fn a_queued_review_keeps_the_path_as_written() {
        let asked = target(Platform::GitHub, "Owner/Repo", 7);
        let key = Key::of(&asked);
        assert_eq!(key, Key::of(&target(Platform::GitHub, "owner/repo", 7)));
        let active = Active {
            run: new_run_id(),
            commit: CommitSha::parse(&"a".repeat(40)).unwrap(),
            cancel: CancellationToken::new(),
            generation: 1,
            repo: asked.repo.path(),
            since: time::OffsetDateTime::now_utc(),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            trigger: "push".to_owned(),
            requester: None,
        };
        assert_eq!(waiting(&key, &active).repo, "Owner/Repo");
    }
}
