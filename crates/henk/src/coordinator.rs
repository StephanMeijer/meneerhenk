//! Runs reviews, rebases and plans in the background, one run per
//! pull/merge request at a time, rebases first (§5.2, #580; open question 2
//! decided in `henk_domain::queue`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use henk_domain::allowlist::Platform;
use henk_domain::queue::{Decision, Priority, Work, decide};
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_domain::run::{RunId, RunKind};
use henk_platform::{IssueTarget, ReviewHandle, ReviewTarget};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::app::App;
use crate::execute::{ExecuteRequest, run_execute};
use crate::ids::new_run_id;
use crate::issue_runs::Comments;
use crate::plan::{PlanRequest, run_plan};
use crate::rebase::{RebaseRequest, run_rebase};
use crate::review::{
    CancelledBy, Interrupted, ReviewReport, ReviewRequest, Superseded,
    report_cancelled_while_queued, report_interrupted_while_queued, run_review,
};
use crate::slot_queue::{NotWaiting, Slot, SlotQueue};
use crate::ticket::{Limits, Live, TicketRequest, run_ticket};

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

/// One issue, as the coordinator tells ticket runs apart: like [`Key`], for
/// an issue (#536). A pull request and an issue of one number are not the
/// same thing, so it is not [`Key`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct IssueKey {
    platform: Platform,
    repo: String,
    number: u64,
}

impl IssueKey {
    fn of(target: &IssueTarget) -> Self {
        Self {
            platform: target.repo.platform(),
            repo: target.repo.path().to_ascii_lowercase(),
            number: target.number,
        }
    }
}

/// What a request for a ticket run came to (#536).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// A ticket run started.
    Started(RunId),
    /// The issue already has one going; this is its id.
    Joined(RunId),
}

/// Keeps an issue's place among the running ticket runs until dropped, so
/// it frees however the run ends.
struct TicketPlace {
    tickets: Arc<Mutex<HashMap<IssueKey, RunId>>>,
    key: IssueKey,
}

impl Drop for TicketPlace {
    fn drop(&mut self) {
        self.tickets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[derive(Debug)]
struct Active {
    run: RunId,
    /// What it does: a review of a commit, or a rebase.
    work: Work,
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
    /// Closed once its task has ended: a run that replaces it takes a slot
    /// only then, so two runs never hold slots on one pull request (#580).
    ended: watch::Receiver<()>,
}

/// Who asked for a run, as [`Active`] keeps it.
struct Asked {
    repo: String,
    trigger: String,
    requester: Option<String>,
}

impl Active {
    /// A run on a pull request, taken now, and the sender whose drop says
    /// it ended.
    fn new(
        run: RunId,
        work: Work,
        generation: u64,
        cancel: CancellationToken,
        asked: Asked,
    ) -> (Self, watch::Sender<()>) {
        let (ended_tx, ended) = watch::channel(());
        let active = Self {
            run,
            work,
            cancel,
            generation,
            repo: asked.repo,
            since: time::OffsetDateTime::now_utc(),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            trigger: asked.trigger,
            requester: asked.requester,
            ended,
        };
        (active, ended_tx)
    }
}

/// A review asked for while a rebase of its pull request runs (#580): it
/// starts once the rebase ends, unless the rebase pushed, and the review
/// of the rebased head takes its place.
#[derive(Debug)]
struct Held {
    run: RunId,
    request: ReviewRequest,
    commit: CommitSha,
    since: time::OffsetDateTime,
}

/// Waits until the run `predecessor` belongs to has ended, unless `cancel`
/// fires first; true when it ended (#580).
async fn after(predecessor: Option<&watch::Receiver<()>>, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        () = ended(predecessor) => true,
    }
}

/// Waits until the run `predecessor` belongs to has ended.
async fn ended(predecessor: Option<&watch::Receiver<()>>) {
    if let Some(mut ended) = predecessor.cloned() {
        // Nothing is ever sent: the sender's drop is the end.
        while ended.changed().await.is_ok() {}
    }
}

/// The slots for runs on pull requests: how many there are, how many are taken, and what
/// waits for one (#225).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slots {
    /// `review.max_concurrent`.
    pub limit: usize,
    /// Slots a review or a rebase holds now.
    pub in_use: usize,
    /// Runs waiting for a slot, in the order they will start, then reviews
    /// waiting for a rebase of their pull request.
    pub waiting: Vec<Waiting>,
}

/// Why a run waits (#251).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Every slot is taken, or a rebase waits ahead of it. A review never
    /// waits behind another of the same pull request: a new commit
    /// supersedes it, the same commit joins it.
    NoSlot,
    /// A rebase of its pull request runs or waits; it starts after that
    /// (#580).
    AfterRebase,
}

impl Reason {
    /// The API's word for it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSlot => "no_slot",
            Self::AfterRebase => "after_rebase",
        }
    }
}

/// A run waiting for a slot, or a review waiting for a rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// The run it will be; no record exists until it starts.
    pub run: RunId,
    /// A review or a rebase.
    pub kind: RunKind,
    /// Where it waits: a rebase ahead of every review (#580).
    pub priority: Priority,
    /// Where the pull or merge request is.
    pub platform: Platform,
    /// `owner/name`.
    pub repo: String,
    /// The pull or merge request.
    pub number: u64,
    /// The commit it will review; none for a rebase.
    pub commit: Option<CommitSha>,
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
    /// The rebase it waits for, when it waits for one.
    pub waits_for: Option<RunId>,
}

/// A waiting run as the queue shows it; its position is set by the caller.
fn waiting(key: &Key, active: &Active) -> Waiting {
    Waiting {
        run: active.run.clone(),
        kind: match active.work {
            Work::Review(_) => RunKind::Review,
            Work::Rebase => RunKind::Rebase,
        },
        priority: active.work.priority(),
        platform: key.platform,
        repo: active.repo.clone(),
        number: key.number,
        commit: active.work.commit().cloned(),
        trigger: active.trigger.clone(),
        requester: active.requester.clone(),
        since: active.since,
        position: 0,
        reason: Reason::NoSlot,
        waits_for: None,
    }
}

/// A review held for a rebase as the queue shows it.
fn held(key: &Key, held: &Held, rebase: Option<&Active>) -> Waiting {
    let request = &held.request;
    Waiting {
        run: held.run.clone(),
        kind: RunKind::Review,
        priority: Priority::Review,
        platform: key.platform,
        repo: request.target.repo.path(),
        number: key.number,
        commit: Some(held.commit.clone()),
        trigger: request.trigger.clone(),
        requester: request.requester.clone(),
        since: held.since,
        position: 0,
        reason: Reason::AfterRebase,
        waits_for: rebase.map(|a| a.run.clone()),
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
pub(crate) async fn queue_check(app: &App, request: &ReviewRequest) -> Option<ReviewHandle> {
    let (Some(run), Some(commit)) = (&request.run, &request.commit) else {
        return None;
    };
    let reviews = app.settings.review.r#loop.is_some();
    if !app.settings.allowlist.allows(&request.target.repo) || !reviews {
        return None;
    }
    let writer = app.writer(&request.target.repo).ok()?;
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
/// (#262). The review is in the slot queue from when it was submitted, so
/// a slow platform costs it no place: reviews start in the queue's order,
/// as [`Coordinator::slots`] says, which a colleague may change (#401). A
/// cancel ends the wait at once, not once a slot frees. Whatever ends it,
/// the queued check is in `request` before this returns, so it closes.
/// `None` when cancelled.
async fn wait_for_slot(
    app: &App,
    slots: &Arc<SlotQueue>,
    run: &RunId,
    cancel: &CancellationToken,
    request: &mut ReviewRequest,
) -> Option<Slot> {
    let (waited, queued_check) = {
        // Boxed so a cancelled review leaves the queue before the platform
        // answers.
        let mut acquire = Box::pin(slots.wait(run));
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
/// it (#160). Returns how it ended, as `run_review` would have said it.
async fn ended_while_queued(app: &App, run: &RunId, request: &ReviewRequest) -> anyhow::Error {
    if let Some(by) = app.cancels.cancelled_by(run) {
        if let Err(error) = report_cancelled_while_queued(app, request, &by).await {
            warn!(%error, "could not record the cancelled review");
        }
        return CancelledBy(by).into();
    }
    if app.cancels.superseded_by(run).is_none() && app.shutdown.is_cancelled() {
        // Henk stops: the run is recorded as interrupted, so the next
        // start resumes it like a review that was running (#160).
        if let Err(error) = report_interrupted_while_queued(app, request).await {
            warn!(%error, "could not record the interrupted review");
        }
        return Interrupted.into();
    }
    let superseded =
        app.cancels.superseded_for_rebase(run) || app.cancels.superseded_by(run).is_some();
    let ended: anyhow::Error = if superseded {
        Superseded.into()
    } else {
        Interrupted.into()
    };
    let (Some(handle), Some(commit)) = (&request.queued_check, &request.commit) else {
        info!("review stopped before it started");
        return ended;
    };
    let outcome = if app.cancels.superseded_for_rebase(run) {
        info!("review superseded by a rebase before it started");
        ReviewOutcome::superseded_by_rebase(commit.clone())
    } else if app.cancels.superseded_by(run).is_some() {
        info!("review superseded before it started");
        ReviewOutcome::superseded(commit.clone())
    } else {
        info!("review stopped with Henk before it started");
        ReviewOutcome::interrupted(commit.clone())
    };
    let link = app.settings.queue_link(run);
    match app.writer(&request.target.repo) {
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
    ended
}

/// Starts and tracks background runs.
pub struct Coordinator {
    app: Arc<App>,
    active: Arc<Mutex<HashMap<Key, Active>>>,
    /// Reviews waiting for a rebase of their pull request (#580). Locked
    /// after `active`, never before.
    held: Mutex<HashMap<Key, Held>>,
    /// Runs cancelled by a person while they may still be ending: the next
    /// run on their pull request takes a slot only once they have (#580).
    /// Locked after `active`, never before.
    ending: Mutex<HashMap<Key, watch::Receiver<()>>>,
    /// The ticket runs going, one per issue (#536).
    tickets: Arc<Mutex<HashMap<IssueKey, RunId>>>,
    /// The slots for runs on pull requests, and the order runs wait for
    /// them in (#401, #580).
    review_slots: Arc<SlotQueue>,
    generation: Mutex<u64>,
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
        let slots_changed = Arc::new(watch::Sender::new(()));
        Self {
            review_slots: SlotQueue::new(
                app.settings.review.max_concurrent,
                Arc::clone(&slots_changed),
            ),
            app,
            active: Arc::new(Mutex::new(HashMap::new())),
            held: Mutex::new(HashMap::new()),
            ending: Mutex::new(HashMap::new()),
            tickets: Arc::new(Mutex::new(HashMap::new())),
            generation: Mutex::new(0),
            slots_changed,
        }
    }

    /// The application.
    #[must_use]
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// Runs on pull requests running or waiting for a slot right now: what
    /// a shutdown waits for, as both hear its token.
    #[must_use]
    pub fn tracked_reviews(&self) -> usize {
        self.active.lock().map_or(0, |m| m.len())
    }

    /// Ticket runs going right now: what a shutdown waits for, so each
    /// writes its ending (#536).
    #[must_use]
    pub fn tracked_tickets(&self) -> usize {
        self.tickets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Runs on pull requests that hold a slot right now (#251: a waiting
    /// one does not count as running).
    #[must_use]
    pub fn running_reviews(&self) -> usize {
        self.active.lock().map_or(0, |m| {
            m.values()
                .filter(|a| a.started.load(std::sync::atomic::Ordering::Relaxed))
                .count()
        })
    }

    /// Runs waiting for a slot right now, and reviews waiting for a rebase.
    #[must_use]
    pub fn queued_reviews(&self) -> usize {
        let queued = self.active.lock().map_or(0, |m| {
            m.values()
                .filter(|a| !a.started.load(std::sync::atomic::Ordering::Relaxed))
                .count()
        });
        queued + self.held.lock().map_or(0, |h| h.len())
    }

    /// The slots and what waits for one.
    #[must_use]
    pub fn slots(&self) -> Slots {
        let (limit, in_use) = self.review_slots.usage();
        // In the queue's order: the order they start in (#401).
        let order = self.review_slots.order();
        let mut waiting: Vec<Waiting> = self.active.lock().map_or_else(
            |_| Vec::new(),
            |active| {
                let mut waiting: Vec<Waiting> = order
                    .iter()
                    .filter_map(|run| {
                        active
                            .iter()
                            .find(|(_, a)| {
                                a.run == *run
                                    && !a.started.load(std::sync::atomic::Ordering::Relaxed)
                            })
                            .map(|(key, a)| waiting(key, a))
                    })
                    .collect();
                // Then the reviews that start after a rebase, oldest first.
                if let Ok(holding) = self.held.lock() {
                    let mut after: Vec<Waiting> = holding
                        .iter()
                        .map(|(key, h)| held(key, h, active.get(key)))
                        .collect();
                    after.sort_by_key(|w| w.since);
                    waiting.extend(after);
                }
                waiting
            },
        );
        for (at, entry) in waiting.iter_mut().enumerate() {
            entry.position = at + 1;
        }
        Slots {
            limit,
            in_use,
            waiting,
        }
    }

    /// Takes a slot as a run would, so a test can fill them.
    #[cfg(test)]
    pub(crate) fn take_a_slot(&self) -> Option<Slot> {
        self.review_slots.take_any()
    }

    /// Wakes when a run starts waiting, takes or frees a slot, or leaves
    /// the queue; [`Coordinator::slots`] then says how things stand.
    #[must_use]
    pub fn watch_slots(&self) -> watch::Receiver<()> {
        self.slots_changed.subscribe()
    }

    fn next_generation(&self) -> u64 {
        self.generation.lock().map_or(0, |mut counter| {
            *counter += 1;
            *counter
        })
    }

    /// Makes `run` the one run on its pull request, in place of any other
    /// (#580). Returns its cancellation token, whether it started, and the
    /// sender whose drop says it ended.
    fn track(
        &self,
        active: &mut HashMap<Key, Active>,
        key: Key,
        tracked: (RunId, Work, u64),
        asked: Asked,
    ) -> (
        CancellationToken,
        Arc<std::sync::atomic::AtomicBool>,
        watch::Sender<()>,
    ) {
        let (run, work, generation) = tracked;
        // A child of the shutdown token: a shutdown reaches it too.
        let cancel = self.app.shutdown.child_token();
        let (entry, ended_tx) = Active::new(run, work, generation, cancel.clone(), asked);
        let started = Arc::clone(&entry.started);
        active.insert(key, entry);
        (cancel, started, ended_tx)
    }

    /// Holds a review as `run` until the rebase of its pull request ends
    /// (#580), in place of one held before.
    fn hold(&self, key: Key, run: &RunId, request: ReviewRequest, commit: CommitSha) {
        info!(repo = %request.target.repo.path(), number = key.number, run = %run, "a review waits for the rebase of its pull request");
        let waits = Held {
            run: run.clone(),
            request,
            commit,
            since: time::OffsetDateTime::now_utc(),
        };
        if let Ok(mut holding) = self.held.lock()
            && let Some(older) = holding.insert(key, waits)
        {
            info!(older = %older.run, newer = %run, "a newer review replaces one waiting for a rebase");
        }
        self.slots_changed.send_replace(());
    }

    /// Forgets the review held for a rebase of the pull request, when a
    /// newer review takes it as `newer`: the rebase it waited for may have
    /// been cancelled, and the held one would otherwise start later and
    /// supersede the newer review with an older commit. Called with
    /// `active` locked.
    fn drop_held(&self, key: &Key, newer: &RunId) {
        if let Ok(mut holding) = self.held.lock()
            && let Some(older) = holding.remove(key)
        {
            info!(older = %older.run, newer = %newer, "a newer review replaces one waiting for a rebase");
        }
    }

    /// Records that a request joined `run`, off the lock: the store is
    /// async and the coordinator's guard is not.
    fn record_joined(&self, run: &RunId, trigger: &str) {
        let store = Arc::clone(&self.app.store);
        let (run, trigger) = (run.clone(), trigger.to_owned());
        tokio::spawn(async move {
            let _ = store.joined(&run, &trigger).await;
        });
    }

    /// What the last run cancelled on the pull request may still be doing
    /// (#580): the next run there waits for it to end.
    fn still_ending(&self, key: &Key) -> Option<watch::Receiver<()>> {
        self.ending.lock().ok().and_then(|mut e| e.remove(key))
    }

    /// Decides what to do with a review request for `commit` and acts on it.
    /// Returns the decision and the run it concerns: the new run, the one
    /// joined, or the one that will start after a rebase.
    pub fn submit_review(&self, request: ReviewRequest, commit: CommitSha) -> (Decision, RunId) {
        self.submit_review_as(request, commit, new_run_id(), None)
    }

    /// [`Self::submit_review`] for a caller that waits for the review to
    /// end. The receiver gets how the run ended, as `run_review` returns it
    /// (an error downcasts to `Superseded`, `CancelledBy`, `Interrupted` or
    /// `NotReviewable`), once the run has left the queue and freed its slot.
    ///
    /// It answers only when the decision is `Start` or `Supersede`. On
    /// `Join` and `After` the review is another run's, which nobody here
    /// waits on: the receiver is closed and the run id names that run. A
    /// closed receiver after `Start` means the coordinator refused or the
    /// run's task died, and nothing says how it ended.
    pub fn submit_review_awaited(
        &self,
        request: ReviewRequest,
        commit: CommitSha,
    ) -> (
        Decision,
        RunId,
        oneshot::Receiver<anyhow::Result<ReviewReport>>,
    ) {
        let (tell, ended) = oneshot::channel();
        let (decision, run) = self.submit_review_as(request, commit, new_run_id(), Some(tell));
        (decision, run, ended)
    }

    /// [`Self::submit_review`] as `run`: a review held for a rebase keeps
    /// the run id it was given then. `tell` hears how the run ended, when
    /// this call starts one.
    fn submit_review_as(
        &self,
        request: ReviewRequest,
        commit: CommitSha,
        run: RunId,
        tell: Option<oneshot::Sender<anyhow::Result<ReviewReport>>>,
    ) -> (Decision, RunId) {
        let key = Key::of(&request.target);
        let generation = self.next_generation();
        let (decision, cancel, started, ended_tx, predecessor) = {
            let Ok(mut active) = self.active.lock() else {
                error!("coordinator lock poisoned");
                return (Decision::Start, run);
            };
            let work = Work::Review(commit.clone());
            let decision = decide(active.get(&key).map(|a| &a.work), &work);
            let mut predecessor = None;
            match decision {
                Decision::Join => {
                    let joined = active
                        .get(&key)
                        .map_or_else(|| run.clone(), |a| a.run.clone());
                    info!(repo = %request.target.repo.path(), number = key.number, run = %joined, "joined the running review");
                    self.record_joined(&joined, &request.trigger);
                    return (Decision::Join, joined);
                }
                Decision::After => {
                    self.hold(key, &run, request, commit);
                    return (Decision::After, run);
                }
                Decision::Supersede => {
                    if let Some(old) = active.remove(&key) {
                        warn!(repo = %request.target.repo.path(), number = key.number, new = %commit.short(), "superseding a running review");
                        // A review still waiting hands its place in the
                        // queue to the one that replaces it (#401).
                        self.review_slots.replace(&old.run, &run, Priority::Review);
                        // The old run learns which run replaced it (#231).
                        if !self.app.cancels.supersede(&old.run, &run) {
                            old.cancel.cancel();
                        }
                        predecessor = Some(old.ended);
                    }
                    self.drop_held(&key, &run);
                }
                Decision::Start => {
                    predecessor = self.still_ending(&key);
                    self.drop_held(&key, &run);
                }
            }
            let asked = Asked {
                repo: request.target.repo.path(),
                trigger: request.trigger.clone(),
                requester: request.requester.clone(),
            };
            let (cancel, started, ended_tx) = self.track(
                &mut active,
                key.clone(),
                (run.clone(), work, generation),
                asked,
            );
            (decision, cancel, started, ended_tx, predecessor)
        };
        // In line from now: the queue's order is the order of the requests.
        self.review_slots.join(&run, Priority::Review);
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
        let queued = run.clone();
        tokio::spawn(async move {
            let _cancellable = cancellable;
            // Dropped last: the end of this task is the end of the run.
            let _ended = ended_tx;
            let mut request = request;
            // A run that replaces another takes a slot once that one has
            // ended (#580). Then a free slot with nobody before it starts
            // the review at once, as before; otherwise the pull request
            // shows it queued while it waits (#262).
            let permit = if after(predecessor.as_ref(), &cancel).await {
                match slots.try_take(&queued) {
                    Some(slot) => Some(slot),
                    None => wait_for_slot(&app, &slots, &queued, &cancel, &mut request).await,
                }
            } else {
                None
            };
            // Out of the queue however the wait ended.
            slots.leave(&queued);
            started.store(true, std::sync::atomic::Ordering::Relaxed);
            slots_changed.send_replace(());
            let result = if cancel.is_cancelled() {
                Err(ended_while_queued(&app, &queued, &request).await)
            } else {
                let result = run_review(&app, request, cancel).await;
                if let Err(error) = &result {
                    warn!(%error, "review ended with an error");
                }
                result
            };
            drop(permit);
            // Not ended before the run it replaced has.
            ended(predecessor.as_ref()).await;
            leave(&active, &key, generation, &slots_changed);
            // Told last: whoever waits may ask for the next review of the
            // pull request, which then finds this one gone.
            if let Some(tell) = tell {
                let _ = tell.send(result);
            }
        });
        (decision, run)
    }

    /// Cancels a running or waiting review, rebase or plan on behalf of
    /// `by` (#69). A cancelled run leaves the running set at once, so a new
    /// request for the same pull request starts afresh instead of joining
    /// it, once the cancelled one has ended (#580). False when the run is
    /// not one this process is running.
    pub fn cancel(&self, run: &RunId, by: String) -> bool {
        let held = self.held.lock().is_ok_and(|mut holding| {
            let before = holding.len();
            holding.retain(|_, h| h.run != *run);
            holding.len() != before
        });
        if held {
            info!(run = %run, by, "a review waiting for a rebase was cancelled");
            self.slots_changed.send_replace(());
            return true;
        }
        if !self.app.cancels.cancel(run, by) {
            return false;
        }
        if let Ok(mut active) = self.active.lock() {
            let key = active
                .iter()
                .find(|(_, a)| a.run == *run)
                .map(|(key, _)| key.clone());
            if let Some(key) = key
                && let Some(cancelled) = active.remove(&key)
                && let Ok(mut ending) = self.ending.lock()
            {
                ending.insert(key, cancelled.ended);
            }
        }
        self.review_slots.leave(run);
        self.slots_changed.send_replace(());
        true
    }

    /// Moves a run waiting for a slot to `position` in the queue, 1 being
    /// the next to start, on behalf of a colleague (#401); never ahead of
    /// a waiting rebase, nor a rebase behind a review (#580). Returns
    /// where it was and where it is now.
    ///
    /// # Errors
    ///
    /// [`NotWaiting`] when the run is not one waiting here for a slot: it
    /// started, ended, waits for a rebase, or another process has it.
    pub fn move_queued(&self, run: &RunId, position: usize) -> Result<(usize, usize), NotWaiting> {
        let waiting = self.active.lock().is_ok_and(|active| {
            active
                .values()
                .any(|a| a.run == *run && !a.started.load(std::sync::atomic::Ordering::Relaxed))
        });
        if !waiting {
            return Err(NotWaiting);
        }
        let moved = self.review_slots.move_to(run, position)?;
        info!(run = %run, from = moved.0, to = moved.1, "a waiting run moved in the queue");
        self.slots_changed.send_replace(());
        Ok(moved)
    }

    /// Starts an execute run in the background and returns its run id
    /// (#329). When it opens a pull request, the pull request is reviewed in
    /// a run of its own, with fresh sessions (#321): Henk asks for that
    /// review himself, since the platform's event about his own pull
    /// request is dropped as his own (§8.1).
    pub fn submit_execute(
        self: &Arc<Self>,
        target: IssueTarget,
        note: Option<String>,
        trigger: String,
        requester: Option<String>,
    ) -> RunId {
        let run = new_run_id();
        let request = ExecuteRequest {
            target,
            note,
            trigger,
            run: Some(run.clone()),
            requester: requester.clone(),
            comments: Comments::Own,
        };
        let coordinator = Arc::clone(self);
        let cancel = self.app.shutdown.child_token();
        let cancellable = self.app.cancels.register(run.clone(), cancel.clone());
        tokio::spawn(async move {
            let report = run_execute(&coordinator.app, request, cancel).await;
            drop(cancellable);
            match report {
                Ok(report) => {
                    let commit = report.pull.commit.clone();
                    coordinator.submit_review(report.review(requester), commit);
                }
                Err(error) => warn!(%error, "execute run ended with an error"),
            }
        });
        run
    }

    /// Starts a rebase of a pull request onto its base branch in the
    /// background (#579), as the only run on it and ahead of every review
    /// waiting for a slot (#580). A review of the pull request running or
    /// waiting is superseded; a second rebase request joins the first.
    /// Returns the decision and the run it concerns.
    ///
    /// When it pushes, the rebased head is reviewed in a run of its own:
    /// Henk asks for that review himself, since the platform's event about
    /// his own push is dropped as his own (§8.1). When it does not, a review
    /// asked for while it ran starts then.
    pub fn submit_rebase(
        self: &Arc<Self>,
        target: ReviewTarget,
        trigger: String,
        requester: Option<String>,
    ) -> (Decision, RunId) {
        self.submit_rebase_as(target, trigger, requester, None)
    }

    /// [`Self::submit_rebase`] for a rebase Henk starts himself because the
    /// base branch moved (#579): nobody asked, and `seen` is the head of the
    /// branch he saw behind it.
    pub fn submit_rebase_automatic(
        self: &Arc<Self>,
        target: ReviewTarget,
        trigger: String,
        seen: CommitSha,
    ) -> (Decision, RunId) {
        self.submit_rebase_as(target, trigger, None, Some(seen))
    }

    fn submit_rebase_as(
        self: &Arc<Self>,
        target: ReviewTarget,
        trigger: String,
        requester: Option<String>,
        automatic: Option<CommitSha>,
    ) -> (Decision, RunId) {
        let key = Key::of(&target);
        let generation = self.next_generation();
        let run = new_run_id();
        let (decision, cancel, started, ended_tx, predecessor) = {
            let Ok(mut active) = self.active.lock() else {
                error!("coordinator lock poisoned");
                return (Decision::Start, run);
            };
            let decision = decide(active.get(&key).map(|a| &a.work), &Work::Rebase);
            let mut predecessor = None;
            match decision {
                Decision::Join => {
                    let joined = active
                        .get(&key)
                        .map_or_else(|| run.clone(), |a| a.run.clone());
                    info!(repo = %target.repo.path(), number = key.number, run = %joined, "joined the waiting or running rebase");
                    return (Decision::Join, joined);
                }
                Decision::Supersede => {
                    if let Some(old) = active.remove(&key) {
                        warn!(repo = %target.repo.path(), number = key.number, old = %old.run, "a rebase supersedes the review");
                        // A review still waiting hands its place to the
                        // rebase, which moves up to where rebases wait.
                        self.review_slots.replace(&old.run, &run, Priority::Rebase);
                        if !self.app.cancels.supersede_for_rebase(&old.run, &run) {
                            old.cancel.cancel();
                        }
                        predecessor = Some(old.ended);
                    }
                }
                Decision::Start | Decision::After => predecessor = self.still_ending(&key),
            }
            let asked = Asked {
                repo: target.repo.path(),
                trigger: trigger.clone(),
                requester: requester.clone(),
            };
            let (cancel, started, ended_tx) = self.track(
                &mut active,
                key.clone(),
                (run.clone(), Work::Rebase, generation),
                asked,
            );
            (decision, cancel, started, ended_tx, predecessor)
        };
        self.review_slots.join(&run, Priority::Rebase);
        self.slots_changed.send_replace(());

        let request = RebaseRequest {
            target,
            trigger,
            run: Some(run.clone()),
            requester: requester.clone(),
            automatic,
        };
        let coordinator = Arc::clone(self);
        let cancellable = self.app.cancels.register(run.clone(), cancel.clone());
        let queued = run.clone();
        tokio::spawn(async move {
            let _ended = ended_tx;
            let slots = Arc::clone(&coordinator.review_slots);
            let permit = if after(predecessor.as_ref(), &cancel).await {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => None,
                    slot = slots.wait(&queued) => Some(slot),
                }
            } else {
                None
            };
            slots.leave(&queued);
            started.store(true, std::sync::atomic::Ordering::Relaxed);
            coordinator.slots_changed.send_replace(());
            // Cancelled before it had a slot, it still runs, to record that
            // and say so; it stops before it reads anything.
            let report = run_rebase(&coordinator.app, request, cancel).await;
            drop(permit);
            drop(cancellable);
            ended(predecessor.as_ref()).await;
            leave(
                &coordinator.active,
                &key,
                generation,
                &coordinator.slots_changed,
            );
            let rebased = match report {
                Ok(report) => report.review(requester),
                Err(error) => {
                    warn!(%error, "rebase ended with an error");
                    None
                }
            };
            coordinator.review_after_rebase(&key, rebased);
        });
        (decision, run)
    }

    /// Once a rebase of the pull request has ended: the review of the head
    /// it pushed, or else the review asked for while it ran, if any.
    fn review_after_rebase(&self, key: &Key, rebased: Option<ReviewRequest>) {
        let waited = self
            .held
            .lock()
            .ok()
            .and_then(|mut holding| holding.remove(key));
        match (rebased, waited) {
            (Some(review), waited) => {
                if let Some(waited) = waited {
                    info!(run = %waited.run, "the review of the rebased head replaces a review of the old one");
                }
                if let Some(commit) = review.commit.clone() {
                    self.submit_review(review, commit);
                }
            }
            (None, Some(waited)) => {
                self.submit_review_as(waited.request, waited.commit, waited.run, None);
            }
            (None, None) => {}
        }
    }

    /// Starts an autonomous ticket run in the background (#536): the issue
    /// goes through a plan, an execute run and a review, with nobody
    /// starting each step. An issue has one at a time: asked again while it
    /// runs, the request joins it.
    ///
    /// # Errors
    ///
    /// Returns an error when autonomous runs are not configured.
    pub fn submit_ticket(
        self: &Arc<Self>,
        target: IssueTarget,
        note: Option<String>,
        trigger: String,
        requester: Option<String>,
    ) -> anyhow::Result<Submitted> {
        let limits = Limits::from_settings(&self.app.settings)?;
        let key = IssueKey::of(&target);
        let run = new_run_id();
        let place = {
            let mut tickets = self.tickets.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(going) = tickets.get(&key) {
                info!(repo = %target.repo.path(), issue = target.number, run = %going, "joined the running ticket run");
                self.record_joined(going, &trigger);
                return Ok(Submitted::Joined(going.clone()));
            }
            tickets.insert(key.clone(), run.clone());
            TicketPlace {
                tickets: Arc::clone(&self.tickets),
                key,
            }
        };
        let request = TicketRequest {
            target,
            note,
            trigger,
            run: Some(run.clone()),
            requester,
        };
        let coordinator = Arc::clone(self);
        let cancel = self.app.shutdown.child_token();
        let cancellable = self.app.cancels.register(run.clone(), cancel.clone());
        tokio::spawn(async move {
            // Dropped last: the end of this task is the end of the ticket run.
            let _place = place;
            let _cancellable = cancellable;
            let stages = Live::new(Arc::clone(&coordinator));
            if let Err(error) =
                run_ticket(coordinator.app(), &stages, request, cancel, limits).await
            {
                warn!(%error, "autonomous run ended with an error");
            }
        });
        Ok(Submitted::Started(run))
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
            comments: Comments::Own,
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
        let commit = CommitSha::parse(&"a".repeat(40)).unwrap();
        let (active, _ended) = Active::new(
            new_run_id(),
            Work::Review(commit),
            1,
            CancellationToken::new(),
            Asked {
                repo: asked.repo.path(),
                trigger: "push".to_owned(),
                requester: None,
            },
        );
        assert_eq!(waiting(&key, &active).repo, "Owner/Repo");
    }
}
