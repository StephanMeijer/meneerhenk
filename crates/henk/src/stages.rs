//! The stages of a run (#226): where a review, plan or address run is, for
//! the dashboard's pipeline. A stage is recorded as the run reaches it and
//! again when it ends. Recording one never fails a run: a store error is a
//! warning, and the timeline keeps its lines as before.

use std::collections::BTreeMap;

use henk_domain::draft::{DraftId, Verdict};
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

/// Ends the stages of a run a person cancelled: `cancelled by github:1234`,
/// and `more` after it when there is more to say.
pub async fn cancelled(store: &dyn RunStore, run: &RunId, by: &str, more: &str) {
    let line = format!("cancelled by {by}{more}");
    end(store, run, StageState::Skipped, &line, &line).await;
}

/// `1 open thread`, `3 open threads`.
#[must_use]
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
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

/// What the fact-check made of the drafts, in a line:
/// `4 drafts: 2 confirmed, 1 rejected, 1 repeat`.
#[must_use]
pub fn fact_check_line(verdicts: &BTreeMap<DraftId, Verdict>) -> String {
    let (mut confirmed, mut rejected, mut repeats, mut unchecked) = (0, 0, 0, 0);
    for verdict in verdicts.values() {
        match verdict {
            Verdict::Confirmed { .. } => confirmed += 1,
            Verdict::Rejected { .. } => rejected += 1,
            Verdict::SameAs { .. } => repeats += 1,
            Verdict::Unchecked { .. } | Verdict::NoCheck => unchecked += 1,
        }
    }
    let mut parts = vec![
        format!("{confirmed} confirmed"),
        format!("{rejected} rejected"),
    ];
    if repeats > 0 {
        parts.push(format!(
            "{repeats} {}",
            if repeats == 1 { "repeat" } else { "repeats" }
        ));
    }
    if unchecked > 0 {
        parts.push(format!("{unchecked} unchecked"));
    }
    let drafts = verdicts.len();
    format!(
        "{drafts} {}: {}",
        if drafts == 1 { "draft" } else { "drafts" },
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use henk_domain::marker::ModelId;

    use super::*;

    #[test]
    fn the_fact_check_line_counts_each_verdict_and_is_in_style() {
        let by = ModelId::parse("opus").unwrap();
        let mut verdicts = BTreeMap::new();
        verdicts.insert(
            DraftId::parse("d1").unwrap(),
            Verdict::Confirmed {
                by: by.clone(),
                reason: String::new(),
            },
        );
        verdicts.insert(
            DraftId::parse("d2").unwrap(),
            Verdict::Rejected {
                by: by.clone(),
                reason: String::new(),
            },
        );
        verdicts.insert(
            DraftId::parse("d3").unwrap(),
            Verdict::Unchecked { why: String::new() },
        );
        let line = fact_check_line(&verdicts);
        assert_eq!(line, "3 drafts: 1 confirmed, 1 rejected, 1 unchecked");
        assert!(henk_domain::text::is_in_style(&line));
        assert_eq!(
            fact_check_line(&BTreeMap::new()),
            "0 drafts: 0 confirmed, 0 rejected"
        );
    }
}
