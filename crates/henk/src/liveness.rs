//! Runs a dead process left behind (#7). A running review or plan
//! refreshes its heartbeat; on start, Henk closes every run whose heartbeat
//! stopped, and the review's check with it. A run another live process is
//! working on keeps a fresh heartbeat and is never touched.

use std::sync::Arc;
use std::time::Duration;

use henk_domain::allowlist::RepoRef;
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_domain::run::{RunId, RunKind};
use henk_platform::{ReviewHandle, ReviewTarget};
use henk_store::{RunRecord, RunStatus, RunStore};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::app::App;

/// How often a running process says it is alive.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

/// How long a heartbeat may be silent before its run counts as orphaned:
/// six missed heartbeats.
const STALE_AFTER: Duration = Duration::from_mins(3);

/// What an orphaned run and its lanes end with.
const REAPED: &str = "interrupted: the process ended";

/// Keeps a run's heartbeat fresh until dropped.
pub struct KeepAlive(JoinHandle<()>);

impl KeepAlive {
    /// Starts the heartbeat of `run`. `create_run` already set the first.
    pub fn start(store: Arc<dyn RunStore>, run: RunId) -> Self {
        Self(tokio::spawn(async move {
            let mut every = tokio::time::interval(HEARTBEAT_EVERY);
            every.tick().await;
            loop {
                every.tick().await;
                if let Err(error) = store.heartbeat(&run).await {
                    warn!(%error, run = %run, "could not record a heartbeat");
                }
            }
        }))
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.0.abort();
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
        let closed = match app.store.drop_running_lanes(&run.id, REAPED).await {
            Ok(()) => {
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
