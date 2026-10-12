//! The stages of a run (#226): where a review or a plan is, for
//! the dashboard's pipeline. A stage is recorded as the run reaches it and
//! again when it ends. Recording one never fails a run: a store error is a
//! warning, and the timeline keeps its lines as before.

use henk_domain::run::RunId;
use henk_store::{RunStore, Stage, StageState, StageWrite};
use time::OffsetDateTime;
use tracing::warn;

/// Records where `stage` of `run` stands now.
pub async fn mark(
    store: &dyn RunStore,
    run: &RunId,
    which: Stage,
    state: StageState,
    detail: impl Into<String>,
) {
    write(store, run, StageWrite::now(which, state, detail)).await;
}

/// Records `stage` with times of its own: `started` and, when it ended
/// other than now, `ended`.
pub async fn mark_span(
    store: &dyn RunStore,
    run: &RunId,
    which: Stage,
    state: StageState,
    detail: impl Into<String>,
    started: OffsetDateTime,
    ended: Option<OffsetDateTime>,
) {
    let write_it = StageWrite {
        stage: which,
        state,
        detail: detail.into(),
        started_at: Some(started),
        ended_at: ended,
    };
    write(store, run, write_it).await;
}

async fn write(store: &dyn RunStore, run: &RunId, stage: StageWrite) {
    if let Err(error) = store.stage(run, &stage).await {
        warn!(%error, run = %run, stage = stage.stage.as_str(), "could not record a stage");
    }
}

/// Ends the run's stages: any still running fails with `why`, and `done`
/// says how the run ended.
pub async fn end(store: &dyn RunStore, run: &RunId, state: StageState, done: &str, why: &str) {
    if let Err(error) = store.fail_running_stages(run, why).await {
        warn!(%error, run = %run, "could not close the running stages");
    }
    mark(store, run, Stage::Done, state, done).await;
}

/// Ends the stages of a run that ran to its end: `done` says how.
pub async fn finished(store: &dyn RunStore, run: &RunId, done: &str) {
    end(store, run, StageState::Done, done, "the run ended first").await;
}

/// Ends the stages of a run that did not complete; its error says why.
pub async fn failed(store: &dyn RunStore, run: &RunId) {
    end(
        store,
        run,
        StageState::Failed,
        "did not complete; the error says why",
        "did not complete",
    )
    .await;
}

/// The request's stage alone: when it came and who asked.
pub async fn request(
    store: &dyn RunStore,
    run: &RunId,
    submitted: Option<OffsetDateTime>,
    trigger: &str,
    requester: Option<&str>,
) {
    let who = requester.map_or_else(|| trigger.to_owned(), |by| format!("{trigger}, {by}"));
    match submitted {
        Some(at) => {
            mark_span(
                store,
                run,
                Stage::Requested,
                StageState::Done,
                who,
                at,
                Some(at),
            )
            .await;
        }
        None => mark(store, run, Stage::Requested, StageState::Done, who).await,
    }
}

/// The request's stage: when it came and who asked; and, when the run
/// waited for a slot, how long.
pub async fn requested(
    store: &dyn RunStore,
    run: &RunId,
    submitted: Option<OffsetDateTime>,
    trigger: &str,
    requester: Option<&str>,
) {
    request(store, run, submitted, trigger, requester).await;
    let Some(submitted) = submitted else {
        return;
    };
    let waited = OffsetDateTime::now_utc() - submitted;
    let detail = if waited >= time::Duration::seconds(1) {
        "waited for a review slot"
    } else {
        "a review slot was free"
    };
    mark_span(
        store,
        run,
        Stage::Queued,
        StageState::Done,
        detail,
        submitted,
        None,
    )
    .await;
}
