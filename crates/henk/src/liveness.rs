//! Runs a dead process left behind (#7). A running review or plan
//! refreshes its heartbeat; on start, and every minute while serving, Henk
//! closes every run whose heartbeat stopped, and the review's check with it.
//! A run another live process is working on keeps a fresh heartbeat and is
//! never touched, and a run this process is working on is never touched
//! even when its heartbeat lags (#47). Once the first pass is done, the
//! reviews a restart interrupted are resumed (#160).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use henk_domain::allowlist::RepoRef;
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_domain::run::{RunId, RunKind};
use henk_events::EventBus;
use henk_platform::{ReviewHandle, ReviewTarget};
use henk_store::{RunRecord, RunStatus, RunStore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::App;
use crate::resume::Resumer;

/// How often a running process says it is alive.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

/// How long a heartbeat may be silent before its run counts as orphaned:
/// six missed heartbeats.
pub(crate) const STALE_AFTER: Duration = Duration::from_mins(3);

/// How often a serving process looks for orphaned runs. A process that
/// died and was restarted within [`STALE_AFTER`] finds its old run still
/// fresh at start; this catches it soon after (#47).
const REAP_EVERY: Duration = Duration::from_mins(1);

/// What an orphaned run and its lanes end with.
pub(crate) const REAPED: &str = "interrupted: the process ended";

/// The runs this process is working on. The reaper never closes one of
/// them, even when its heartbeat lags behind (a store that was unreachable
/// for a while), since the run is plainly not orphaned.
#[derive(Debug, Default, Clone)]
pub struct LiveRuns(Arc<Mutex<HashSet<RunId>>>);

impl LiveRuns {
    fn insert(&self, run: RunId) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run);
    }

    fn remove(&self, run: &RunId) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(run);
    }

    /// Whether this process is working on `run`.
    pub(crate) fn contains(&self, run: &RunId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(run)
    }
}

/// Keeps a run's heartbeat fresh, and the run in [`LiveRuns`], until
/// dropped.
pub struct KeepAlive {
    beat: JoinHandle<()>,
    live: LiveRuns,
    run: RunId,
}

impl KeepAlive {
    /// Starts the heartbeat of `run`. `create_run` already set the first.
    pub fn start(store: Arc<dyn RunStore>, live: &LiveRuns, run: RunId) -> Self {
        live.insert(run.clone());
        let beating = run.clone();
        let beat = tokio::spawn(async move {
            let run = beating;
            let mut every = tokio::time::interval(HEARTBEAT_EVERY);
            every.tick().await;
            loop {
                every.tick().await;
                if let Err(error) = store.heartbeat(&run).await {
                    warn!(%error, run = %run, "could not record a heartbeat");
                }
            }
        });
        Self {
            beat,
            live: live.clone(),
            run,
        }
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.beat.abort();
        self.live.remove(&self.run);
    }
}

/// Reaps orphaned runs now and every [`REAP_EVERY`] until `cancel`, and
/// resumes the reviews a restart interrupted on `bus` once the first pass
/// is done (#160).
pub fn spawn_reaper(
    app: Arc<App>,
    bus: Arc<EventBus>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let resumer = Resumer::new(bus, STALE_AFTER);
    tokio::spawn(reap_every(
        app,
        cancel,
        REAP_EVERY,
        STALE_AFTER,
        Some(resumer),
    ))
}

/// The reaper's loop: every `every`, closes the runs silent for longer than
/// `stale_after`. The first pass is immediate. After a pass, `resumer`
/// resumes interrupted reviews on the passes it is due, never on every
/// one (#160).
pub(crate) async fn reap_every(
    app: Arc<App>,
    cancel: CancellationToken,
    every: Duration,
    stale_after: Duration,
    mut resumer: Option<Resumer>,
) {
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticks.tick() => {
                let began = tokio::time::Instant::now();
                reap_silent_since(&app, time::OffsetDateTime::now_utc() - stale_after).await;
                if let Some(resumer) = resumer.as_mut() {
                    resumer.after_pass(&app, began).await;
                }
            }
        }
    }
}

/// Closes every run left `running` by a process that died, and the checks
/// of its reviews. Returns how many runs it closed.
pub async fn reap_orphans(app: &App) -> usize {
    reap_silent_since(app, time::OffsetDateTime::now_utc() - STALE_AFTER).await
}

/// Closes the runs still `running` whose last heartbeat is before `cutoff`.
pub(crate) async fn reap_silent_since(app: &App, cutoff: time::OffsetDateTime) -> usize {
    let orphans = match app.store.orphaned_runs(cutoff).await {
        Ok(orphans) => orphans,
        Err(error) => {
            warn!(%error, "could not list interrupted runs");
            return 0;
        }
    };
    let mut reaped = 0;
    for run in orphans {
        if app.live_runs.contains(&run.id) {
            continue;
        }
        let closed = match app.store.drop_running_lanes(&run.id, REAPED).await {
            Ok(()) => {
                crate::stages::end(
                    &*app.store,
                    &run.id,
                    henk_store::StageState::Failed,
                    REAPED,
                    REAPED,
                )
                .await;
                app.store
                    .finish_run(&run.id, RunStatus::Failed, None, Some(REAPED))
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = closed {
            warn!(%error, run = %run.id, "could not close an interrupted run");
            continue;
        }
        reaped += 1;
        if run.kind == RunKind::Review {
            close_check(app, &run).await;
        }
    }
    if reaped > 0 {
        info!(count = reaped, "reaped interrupted runs");
    }
    reaped
}

/// Completes the check of a reaped review. Its failure is logged: the run
/// is closed either way.
async fn close_check(app: &App, run: &RunRecord) {
    let Some(commit) = run.commit.as_deref().and_then(|c| CommitSha::parse(c).ok()) else {
        return;
    };
    let Ok(repo) = RepoRef::parse(run.platform, &run.repo) else {
        warn!(run = %run.id, repo = %run.repo, "an interrupted run names no valid repository");
        return;
    };
    let writer = match app.writer(run.platform) {
        Ok(writer) => writer,
        Err(error) => {
            warn!(%error, run = %run.id, "no writer to close the check of an interrupted run");
            return;
        }
    };
    let target = ReviewTarget {
        repo,
        number: run.target,
    };
    let handle = run.check_id.clone().map(ReviewHandle);
    if let Err(error) = writer
        .finish_review(
            &target,
            &commit,
            handle.as_ref(),
            &ReviewOutcome::interrupted(commit.clone()),
            &run.link,
        )
        .await
    {
        warn!(%error, run = %run.id, "could not close the check of an interrupted run");
    }
}
