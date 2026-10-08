//! What changes in the run store, as it happens (#202). [`Announcing`]
//! wraps the store Henk writes to and, after each write the dashboard
//! shows, announces a [`Change`] on a [`Feed`]; the dashboard's streams
//! follow the feed. Nothing that writes needs to know: the store is the
//! one place every lane, tool call, draft and verdict passes through.
//!
//! A write holds the feed's gate shared from the store write to its
//! announcement, and a snapshot holds it alone ([`Feed::settled`]), so a
//! snapshot holds every change up to [`Feed::last`] and none after: no
//! change is both in a snapshot and sent after it. A snapshot is a few
//! reads of one run; writes wait for it that long, never for a follower.
//!
//! The feed lives in this process only. A run another Henk process works
//! on is announced there, not here; the streams read such a run from the
//! store instead.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId};
use henk_store::{
    DayCounts, DayRates, DraftDecision, DraftFilter, DraftGroup, DraftListing, DraftRates,
    DraftRecord, EventFacets, EventFilter, EventRecord, EventWithOutcomes, FindingAction,
    FindingRecord, InboundEvent, LaneRecord, LaneStatus, NewRun, OutcomeRecord, Page, PruneCounts,
    RunFilter, RunRecord, RunStatus, RunStore, Stage, StageRecord, StageState, StageWrite,
    StoreError, ToolCallFilter, ToolCallListing, ToolCallRecord, ToolUsage, TranscriptRecord,
    TranscriptSummary,
};
use time::OffsetDateTime;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, broadcast};
use tracing::warn;

/// How many changes a slow follower may fall behind before it is dropped;
/// it then reconnects and catches up from the replay or a snapshot.
const CHANNEL: usize = 1024;

/// How many recent changes are kept to replay to a follower that
/// reconnects.
const REPLAY: usize = 2048;

/// One change to one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The run.
    pub run: RunId,
    /// Its place in this process's feed, from 1.
    pub seq: u64,
    /// What changed.
    pub kind: ChangeKind,
}

/// What changed, with the record as the store now holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// The run started, ended or got its check.
    Run(RunRecord),
    /// A lane started or ended: every lane of the run.
    Lanes(Vec<LaneRecord>),
    /// A session called a tool.
    ToolCall(ToolCallRecord),
    /// A lane queued a draft, or the check decided one.
    Draft(DraftRecord),
    /// A finding was posted, improved, refused, and so on.
    Finding(FindingRecord),
    /// A line on the run's timeline.
    Event(EventRecord),
    /// A session's conversation was kept.
    Transcript(TranscriptSummary),
    /// A stage began or ended (#226): every stage of the run.
    Stages(Vec<StageRecord>),
    /// The run's process said it is alive, at this time (RFC 3339).
    Heartbeat(String),
}

#[derive(Debug)]
struct Inner {
    epoch: String,
    sender: broadcast::Sender<Arc<Change>>,
    recent: Mutex<Recent>,
    /// Shared by a write between the store and its announcement, alone by
    /// a snapshot.
    gate: RwLock<()>,
}

#[derive(Debug, Default)]
struct Recent {
    seq: u64,
    changes: VecDeque<Arc<Change>>,
}

/// The changes of this process, to follow live and to replay.
#[derive(Debug, Clone)]
pub struct Feed(Arc<Inner>);

impl Default for Feed {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(CHANNEL);
        Self(Arc::new(Inner {
            epoch: epoch(),
            sender,
            recent: Mutex::new(Recent::default()),
            gate: RwLock::new(()),
        }))
    }
}

/// A name for this process's feed, so a follower's last id from another
/// process (or before a restart) is never taken for one of ours.
fn epoch() -> String {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; 6];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_ok() {
        bytes.iter().fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
    } else {
        format!("{:x}", OffsetDateTime::now_utc().unix_timestamp_nanos())
    }
}

impl Feed {
    /// This process's feed name.
    #[must_use]
    pub fn epoch(&self) -> &str {
        &self.0.epoch
    }

    /// Announces a change to `run`.
    pub fn announce(&self, run: &RunId, kind: ChangeKind) {
        let mut recent = self.0.recent.lock().unwrap_or_else(PoisonError::into_inner);
        recent.seq += 1;
        let change = Arc::new(Change {
            run: run.clone(),
            seq: recent.seq,
            kind,
        });
        recent.changes.push_back(Arc::clone(&change));
        while recent.changes.len() > REPLAY {
            recent.changes.pop_front();
        }
        // Sent under the lock, so followers see changes in seq order. No
        // follower is no error.
        let _ = self.0.sender.send(change);
    }

    /// Every change from now on.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Change>> {
        self.0.sender.subscribe()
    }

    /// The changes to `run` after `seq` of feed `epoch`, when this feed
    /// still holds all of them; `None` when it cannot say, and the
    /// follower needs the whole run again.
    #[must_use]
    pub fn since(&self, epoch: &str, seq: u64, run: &RunId) -> Option<Vec<Arc<Change>>> {
        if epoch != self.0.epoch {
            return None;
        }
        let recent = self.0.recent.lock().unwrap_or_else(PoisonError::into_inner);
        if seq > recent.seq {
            return None;
        }
        let oldest = recent.changes.front().map_or(recent.seq + 1, |c| c.seq);
        if seq + 1 < oldest {
            return None;
        }
        Some(
            recent
                .changes
                .iter()
                .filter(|c| c.seq > seq && c.run == *run)
                .cloned()
                .collect(),
        )
    }

    /// Waits until no write is between the store and its announcement,
    /// and holds off new ones while the guard lives: what the store then
    /// holds is exactly what the feed announced up to [`Feed::last`]. For
    /// a snapshot; hold it only for the reads, never while sending.
    pub async fn settled(&self) -> RwLockWriteGuard<'_, ()> {
        self.0.gate.write().await
    }

    /// Held by a write from the store write to its announcement.
    async fn writing(&self) -> RwLockReadGuard<'_, ()> {
        self.0.gate.read().await
    }

    /// The last seq announced.
    #[must_use]
    pub fn last(&self) -> u64 {
        self.0
            .recent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .seq
    }
}

fn now() -> String {
    crate::hooks::now_rfc3339()
}

/// The given time, or now when the caller left it to the store.
fn or_now(at: &str) -> String {
    if at.is_empty() { now() } else { at.to_owned() }
}

/// A [`RunStore`] that announces what it writes on a [`Feed`]. Reads pass
/// straight through. A write that fails announces nothing, and a failed
/// read-back for an announcement is logged, never the write's failure.
#[derive(Debug)]
pub struct Announcing {
    inner: Arc<dyn RunStore>,
    feed: Feed,
    /// Held from a read-back to its announcement, so read-backs are
    /// announced in the order they were read: two lanes ending at once
    /// cannot announce an older read last.
    read_back: tokio::sync::Mutex<()>,
}

impl Announcing {
    /// Wraps `inner`, announcing on `feed`.
    pub fn new(inner: Arc<dyn RunStore>, feed: Feed) -> Self {
        Self {
            inner,
            feed,
            read_back: tokio::sync::Mutex::new(()),
        }
    }

    async fn announce_run(&self, id: &RunId) {
        let _order = self.read_back.lock().await;
        match self.inner.run(id).await {
            Ok(Some(run)) => self.feed.announce(id, ChangeKind::Run(run)),
            Ok(None) => {}
            Err(error) => warn!(%error, run = %id, "could not read a run back to announce it"),
        }
    }

    async fn announce_stages(&self, id: &RunId) {
        let _order = self.read_back.lock().await;
        match self.inner.stages(id).await {
            Ok(stages) => self.feed.announce(id, ChangeKind::Stages(stages)),
            Err(error) => warn!(%error, run = %id, "could not read stages back to announce them"),
        }
    }

    async fn announce_lanes(&self, id: &RunId) {
        let _order = self.read_back.lock().await;
        match self.inner.lanes(id).await {
            Ok(lanes) => self.feed.announce(id, ChangeKind::Lanes(lanes)),
            Err(error) => warn!(%error, run = %id, "could not read lanes back to announce them"),
        }
    }
}

#[async_trait]
impl RunStore for Announcing {
    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.create_run(run).await?;
        self.announce_run(&run.id).await;
        Ok(())
    }

    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.finish_run(id, status, summary, error).await?;
        self.announce_run(id).await;
        Ok(())
    }

    async fn supersede_run(&self, id: &RunId, by: &RunId, reason: &str) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.supersede_run(id, by, reason).await?;
        self.announce_run(id).await;
        Ok(())
    }

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        self.inner.run(id).await
    }

    async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.heartbeat(id).await?;
        // So a run page's "heartbeat 12 s ago" stays true (#226).
        self.feed.announce(id, ChangeKind::Heartbeat(now()));
        Ok(())
    }

    async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.stage(run, write).await?;
        self.announce_stages(run).await;
        Ok(())
    }

    async fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.fail_running_stages(run, reason).await?;
        self.announce_stages(run).await;
        Ok(())
    }

    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        self.inner.stages(run).await
    }

    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
        self.inner.daily_stats(since).await
    }

    async fn daily_draft_rates(
        &self,
        group: DraftGroup,
        filter: &DraftFilter,
    ) -> Result<Vec<DayRates>, StoreError> {
        self.inner.daily_draft_rates(group, filter).await
    }

    async fn count_drafts(&self, filter: &DraftFilter) -> Result<u64, StoreError> {
        self.inner.count_drafts(filter).await
    }

    async fn lanes_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, LaneStatus)>, StoreError> {
        self.inner.lanes_of(runs).await
    }

    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        self.inner.stages_of(runs).await
    }

    async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.set_check(id, check_id).await?;
        self.announce_run(id).await;
        Ok(())
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        self.inner.orphaned_runs(stale_before).await
    }

    async fn drop_running_lanes(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.drop_running_lanes(run, reason).await?;
        self.announce_lanes(run).await;
        Ok(())
    }

    async fn start_lane(&self, run: &RunId, name: &str, model: &str) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.start_lane(run, name, model).await?;
        self.announce_lanes(run).await;
        Ok(())
    }

    async fn finish_lane(
        &self,
        run: &RunId,
        name: &str,
        status: LaneStatus,
        turns: u64,
        input_tokens: u64,
        output_tokens: u64,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner
            .finish_lane(run, name, status, turns, input_tokens, output_tokens, error)
            .await?;
        self.announce_lanes(run).await;
        Ok(())
    }

    async fn lanes(&self, run: &RunId) -> Result<Vec<LaneRecord>, StoreError> {
        self.inner.lanes(run).await
    }

    async fn record_finding(
        &self,
        run: &RunId,
        lane: &str,
        path: &str,
        line_number: u32,
        comment_id: &str,
        action: FindingAction,
    ) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner
            .record_finding(run, lane, path, line_number, comment_id, action)
            .await?;
        self.feed.announce(
            run,
            ChangeKind::Finding(FindingRecord {
                at: now(),
                lane: lane.to_owned(),
                path: path.to_owned(),
                line: line_number,
                comment_id: comment_id.to_owned(),
                action: action.as_str().to_owned(),
            }),
        );
        Ok(())
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        self.inner.findings(run).await
    }

    async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.record_draft(run, draft).await?;
        self.feed.announce(
            run,
            ChangeKind::Draft(DraftRecord {
                at: or_now(&draft.at),
                ..draft.clone()
            }),
        );
        Ok(())
    }

    async fn decide_draft(
        &self,
        run: &RunId,
        draft: &str,
        decision: &DraftDecision,
    ) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.decide_draft(run, draft, decision).await?;
        let _order = self.read_back.lock().await;
        match self.inner.drafts(run).await {
            Ok(drafts) => {
                if let Some(decided) = drafts.into_iter().find(|d| d.draft == draft) {
                    self.feed.announce(run, ChangeKind::Draft(decided));
                }
            }
            Err(error) => {
                warn!(%error, run = %run, draft, "could not read a draft back to announce it");
            }
        }
        Ok(())
    }

    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        self.inner.drafts(run).await
    }

    async fn draft_rates(
        &self,
        group: DraftGroup,
        filter: &DraftFilter,
    ) -> Result<Vec<DraftRates>, StoreError> {
        self.inner.draft_rates(group, filter).await
    }

    async fn list_drafts(
        &self,
        filter: &DraftFilter,
        page: Page,
    ) -> Result<Vec<DraftListing>, StoreError> {
        self.inner.list_drafts(filter, page).await
    }

    async fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.record_tool_call(run, call).await?;
        self.feed.announce(
            run,
            ChangeKind::ToolCall(ToolCallRecord {
                at: or_now(&call.at),
                ..call.clone()
            }),
        );
        Ok(())
    }

    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        self.inner.tool_calls(run).await
    }

    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
        self.inner.tool_usage(filter).await
    }

    async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError> {
        self.inner.list_tool_calls(filter, page).await
    }

    async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.record_transcript(run, transcript).await?;
        self.feed.announce(
            run,
            ChangeKind::Transcript(TranscriptSummary {
                at: or_now(&transcript.at),
                session: transcript.session.clone(),
                model: transcript.model.clone(),
                stop: transcript.stop.clone(),
                turns: transcript.turns,
                bytes: transcript.bytes,
            }),
        );
        Ok(())
    }

    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
        self.inner.transcripts(run).await
    }

    async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        self.inner.transcript(run, session).await
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        let _writing = self.feed.writing().await;
        self.inner.event(run, level, message).await?;
        self.feed.announce(
            run,
            ChangeKind::Event(EventRecord {
                at: now(),
                level: level.to_owned(),
                message: message.to_owned(),
            }),
        );
        Ok(())
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        self.inner.events(run).await
    }

    async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        self.inner.joined(run, source).await
    }

    async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
        self.inner.record_event(event).await
    }

    async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
        self.inner.record_outcome(outcome).await
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        self.inner.inbound_event(id).await
    }

    async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        self.inner.event_facets().await
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        self.inner.outcomes(id).await
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        self.inner.inbound_events_for_run(run).await
    }

    async fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError> {
        self.inner.prune_events(older_than).await
    }

    async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        self.inner.list_runs(filter, page).await
    }

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        self.inner.count_runs(filter).await
    }

    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        self.inner.list_inbound_events(filter, page).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::too_many_lines
    )]

    use henk_domain::allowlist::Platform;
    use henk_domain::run::RunKind;
    use henk_store::{DraftVerdict, SqliteStore};
    use tokio::sync::oneshot;

    use super::*;

    fn id(text: &str) -> RunId {
        RunId::parse(text).unwrap()
    }

    fn new_run(run: &str) -> NewRun {
        NewRun {
            id: id(run),
            kind: RunKind::Review,
            platform: Platform::GitHub,
            repo: "o/r".into(),
            target: 7,
            commit: None,
            requester: None,
            trigger: "opened".into(),
            link: String::new(),
        }
    }

    /// A store that stops once, right after the named call has done its
    /// work, until the test lets it go: a write that is in the store but
    /// not yet announced, or a read-back not yet announced.
    #[derive(Debug)]
    struct Paused {
        inner: SqliteStore,
        at: &'static str,
        pause: std::sync::Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    }

    /// The test's ends of a pause: told when the call stops, and the go.
    struct Pause {
        reached: oneshot::Receiver<()>,
        go: oneshot::Sender<()>,
    }

    impl Paused {
        fn new(at: &'static str) -> Self {
            Self {
                inner: SqliteStore::in_memory().unwrap(),
                at,
                pause: std::sync::Mutex::new(None),
            }
        }

        /// Stops the next call to `at`.
        fn arm(&self) -> Pause {
            let (reached_tx, reached) = oneshot::channel();
            let (go, go_rx) = oneshot::channel();
            *self.pause.lock().unwrap() = Some((reached_tx, go_rx));
            Pause { reached, go }
        }

        async fn hold(&self, call: &str) {
            if call != self.at {
                return;
            }
            let pause = self.pause.lock().unwrap().take();
            if let Some((reached, go)) = pause {
                reached.send(()).unwrap();
                go.await.unwrap();
            }
        }
    }

    #[async_trait]
    impl RunStore for Paused {
        async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
            self.inner.create_run(run).await
        }
        async fn finish_run(
            &self,
            id: &RunId,
            status: RunStatus,
            summary: Option<&str>,
            error: Option<&str>,
        ) -> Result<(), StoreError> {
            self.inner.finish_run(id, status, summary, error).await
        }
        async fn supersede_run(
            &self,
            id: &RunId,
            by: &RunId,
            reason: &str,
        ) -> Result<(), StoreError> {
            self.inner.supersede_run(id, by, reason).await
        }
        async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
            self.inner.run(id).await
        }
        async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
            self.inner.heartbeat(id).await
        }
        async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError> {
            self.inner.stage(run, write).await
        }
        async fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
            self.inner.fail_running_stages(run, reason).await
        }
        async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
            self.inner.stages(run).await
        }
        async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
            self.inner.daily_stats(since).await
        }
        async fn daily_draft_rates(
            &self,
            group: DraftGroup,
            filter: &DraftFilter,
        ) -> Result<Vec<DayRates>, StoreError> {
            self.inner.daily_draft_rates(group, filter).await
        }
        async fn count_drafts(&self, filter: &DraftFilter) -> Result<u64, StoreError> {
            self.inner.count_drafts(filter).await
        }
        async fn lanes_of(
            &self,
            runs: &[RunId],
        ) -> Result<Vec<(RunId, String, LaneStatus)>, StoreError> {
            self.inner.lanes_of(runs).await
        }
        async fn stages_of(
            &self,
            runs: &[RunId],
        ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
            self.inner.stages_of(runs).await
        }
        async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
            self.inner.set_check(id, check_id).await
        }
        async fn orphaned_runs(
            &self,
            stale_before: OffsetDateTime,
        ) -> Result<Vec<RunRecord>, StoreError> {
            self.inner.orphaned_runs(stale_before).await
        }
        async fn drop_running_lanes(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
            self.inner.drop_running_lanes(run, reason).await
        }
        async fn start_lane(&self, run: &RunId, name: &str, model: &str) -> Result<(), StoreError> {
            self.inner.start_lane(run, name, model).await
        }
        async fn finish_lane(
            &self,
            run: &RunId,
            name: &str,
            status: LaneStatus,
            turns: u64,
            input_tokens: u64,
            output_tokens: u64,
            error: Option<&str>,
        ) -> Result<(), StoreError> {
            self.inner
                .finish_lane(run, name, status, turns, input_tokens, output_tokens, error)
                .await
        }
        async fn lanes(&self, run: &RunId) -> Result<Vec<LaneRecord>, StoreError> {
            let lanes = self.inner.lanes(run).await;
            self.hold("lanes").await;
            lanes
        }
        async fn record_finding(
            &self,
            run: &RunId,
            lane: &str,
            path: &str,
            line_number: u32,
            comment_id: &str,
            action: FindingAction,
        ) -> Result<(), StoreError> {
            self.inner
                .record_finding(run, lane, path, line_number, comment_id, action)
                .await
        }
        async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
            self.inner.findings(run).await
        }
        async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError> {
            self.inner.record_draft(run, draft).await
        }
        async fn decide_draft(
            &self,
            run: &RunId,
            draft: &str,
            decision: &DraftDecision,
        ) -> Result<(), StoreError> {
            self.inner.decide_draft(run, draft, decision).await
        }
        async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
            self.inner.drafts(run).await
        }
        async fn record_tool_call(
            &self,
            run: &RunId,
            call: &ToolCallRecord,
        ) -> Result<(), StoreError> {
            self.inner.record_tool_call(run, call).await
        }
        async fn draft_rates(
            &self,
            group: DraftGroup,
            filter: &DraftFilter,
        ) -> Result<Vec<DraftRates>, StoreError> {
            self.inner.draft_rates(group, filter).await
        }
        async fn list_drafts(
            &self,
            filter: &DraftFilter,
            page: Page,
        ) -> Result<Vec<DraftListing>, StoreError> {
            self.inner.list_drafts(filter, page).await
        }
        async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
            self.inner.tool_calls(run).await
        }
        async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
            self.inner.tool_usage(filter).await
        }
        async fn list_tool_calls(
            &self,
            filter: &ToolCallFilter,
            page: Page,
        ) -> Result<Vec<ToolCallListing>, StoreError> {
            self.inner.list_tool_calls(filter, page).await
        }
        async fn record_transcript(
            &self,
            run: &RunId,
            transcript: &TranscriptRecord,
        ) -> Result<(), StoreError> {
            self.inner.record_transcript(run, transcript).await
        }
        async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
            self.inner.transcripts(run).await
        }
        async fn transcript(
            &self,
            run: &RunId,
            session: &str,
        ) -> Result<Option<TranscriptRecord>, StoreError> {
            self.inner.transcript(run, session).await
        }
        async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
            let written = self.inner.event(run, level, message).await;
            self.hold("event").await;
            written
        }
        async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
            self.inner.events(run).await
        }
        async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
            self.inner.joined(run, source).await
        }
        async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
            self.inner.record_event(event).await
        }
        async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
            self.inner.record_outcome(outcome).await
        }
        async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
            self.inner.inbound_event(id).await
        }
        async fn event_facets(&self) -> Result<EventFacets, StoreError> {
            self.inner.event_facets().await
        }
        async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
            self.inner.outcomes(id).await
        }
        async fn inbound_events_for_run(
            &self,
            run: &RunId,
        ) -> Result<Vec<InboundEvent>, StoreError> {
            self.inner.inbound_events_for_run(run).await
        }
        async fn prune_events(
            &self,
            older_than: OffsetDateTime,
        ) -> Result<PruneCounts, StoreError> {
            self.inner.prune_events(older_than).await
        }
        async fn list_runs(
            &self,
            filter: &RunFilter,
            page: Page,
        ) -> Result<Vec<RunRecord>, StoreError> {
            self.inner.list_runs(filter, page).await
        }
        async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
            self.inner.count_runs(filter).await
        }
        async fn list_inbound_events(
            &self,
            filter: &EventFilter,
            page: Page,
        ) -> Result<Vec<EventWithOutcomes>, StoreError> {
            self.inner.list_inbound_events(filter, page).await
        }
    }

    /// An announcing store over [`Paused`].
    fn paused(at: &'static str) -> (Arc<Announcing>, Feed, Arc<Paused>) {
        let feed = Feed::default();
        let inner = Arc::new(Paused::new(at));
        let store = Arc::new(Announcing::new(
            Arc::clone(&inner) as Arc<dyn RunStore>,
            feed.clone(),
        ));
        (store, feed, inner)
    }

    /// Long enough for a task that is not blocked to finish.
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

    #[tokio::test]
    async fn two_lanes_ending_at_once_announce_their_lanes_in_the_order_read() {
        let (store, feed, paused) = paused("lanes");
        let run = id("r-1");
        store.create_run(&new_run("r-1")).await.unwrap();
        store.start_lane(&run, "lane-a", "m").await.unwrap();
        store.start_lane(&run, "lane-b", "m").await.unwrap();
        let pause = paused.arm();
        // The pause is taken by the next read-back: lane-a's, which reads
        // lane-a finished and lane-b running, and stops before announcing.
        let a = tokio::spawn({
            let (store, run) = (Arc::clone(&store), run.clone());
            async move {
                store
                    .finish_lane(&run, "lane-a", LaneStatus::Finished, 1, 1, 1, None)
                    .await
            }
        });
        pause.reached.await.unwrap();
        let mut b = tokio::spawn({
            let (store, run) = (Arc::clone(&store), run.clone());
            async move {
                store
                    .finish_lane(&run, "lane-b", LaneStatus::Finished, 2, 2, 2, None)
                    .await
            }
        });
        assert!(
            tokio::time::timeout(SETTLE, &mut b).await.is_err(),
            "lane-b's read-back waits for lane-a's announcement"
        );
        pause.go.send(()).unwrap();
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();

        let all = feed.since(feed.epoch(), 0, &run).unwrap();
        let Some(ChangeKind::Lanes(last)) = all.last().map(|c| &c.kind) else {
            panic!("{:?}", kinds(&all))
        };
        assert!(
            last.iter().all(|lane| lane.status == LaneStatus::Finished),
            "the last announcement is the latest read: {last:?}"
        );
    }

    #[tokio::test]
    async fn a_settled_feed_holds_no_write_between_the_store_and_its_announcement() {
        let (store, feed, paused) = paused("event");
        let run = id("r-1");
        store.create_run(&new_run("r-1")).await.unwrap();
        let pause = paused.arm();
        let write = tokio::spawn({
            let (store, run) = (Arc::clone(&store), run.clone());
            async move { store.event(&run, "info", "in the store").await }
        });
        pause.reached.await.unwrap();
        let mut snapshot = tokio::spawn({
            let (store, feed, run) = (Arc::clone(&store), feed.clone(), run.clone());
            async move {
                let _settled = feed.settled().await;
                (feed.last(), store.events(&run).await.unwrap())
            }
        });
        assert!(
            tokio::time::timeout(SETTLE, &mut snapshot).await.is_err(),
            "the snapshot waits for the write's announcement"
        );
        pause.go.send(()).unwrap();
        write.await.unwrap().unwrap();
        let (seq, events) = snapshot.await.unwrap();
        assert_eq!(events.len(), 1);
        let announced = feed.since(feed.epoch(), 0, &run).unwrap();
        assert!(
            announced.iter().all(|c| c.seq <= seq),
            "what the snapshot holds is announced at or before its seq, so nothing repeats"
        );
    }

    fn announcing() -> (Announcing, Feed) {
        let feed = Feed::default();
        let store = Announcing::new(Arc::new(SqliteStore::in_memory().unwrap()), feed.clone());
        (store, feed)
    }

    fn kinds(changes: &[Arc<Change>]) -> Vec<&'static str> {
        changes
            .iter()
            .map(|c| match c.kind {
                ChangeKind::Run(_) => "run",
                ChangeKind::Lanes(_) => "lanes",
                ChangeKind::ToolCall(_) => "tool_call",
                ChangeKind::Draft(_) => "draft",
                ChangeKind::Finding(_) => "finding",
                ChangeKind::Event(_) => "event",
                ChangeKind::Transcript(_) => "transcript",
                ChangeKind::Stages(_) => "stages",
                ChangeKind::Heartbeat(_) => "heartbeat",
            })
            .collect()
    }

    #[tokio::test]
    async fn every_write_the_dashboard_shows_is_announced_as_the_store_holds_it() {
        let (store, feed) = announcing();
        let run = id("r-1");
        store.create_run(&new_run("r-1")).await.unwrap();
        store.start_lane(&run, "lane-a", "m").await.unwrap();
        store
            .record_tool_call(
                &run,
                &ToolCallRecord {
                    at: String::new(),
                    session: "lane-a".into(),
                    model: "m".into(),
                    turn: 2,
                    tool: "read_file".into(),
                    origin: "henk".into(),
                    outcome: "ok".into(),
                    arguments: "{}".into(),
                    arguments_len: 2,
                    result_chars: 10,
                    elapsed_ms: 1,
                },
            )
            .await
            .unwrap();
        let draft = DraftRecord {
            at: String::new(),
            draft: "d1".into(),
            lane: "lane-a".into(),
            model: "m".into(),
            kind: "finding".into(),
            path: "a.rs".into(),
            line: 4,
            target: String::new(),
            body: "Wrong.".into(),
            decision: None,
        };
        store.record_draft(&run, &draft).await.unwrap();
        store
            .decide_draft(
                &run,
                "d1",
                &DraftDecision {
                    at: String::new(),
                    verdict: DraftVerdict::Confirmed,
                    checker: "c".into(),
                    reason: "Yes.".into(),
                    same_as: String::new(),
                    comment_id: "c-1".into(),
                },
            )
            .await
            .unwrap();
        store
            .record_finding(&run, "lane-a", "a.rs", 4, "c-1", FindingAction::Posted)
            .await
            .unwrap();
        store.event(&run, "info", "a line").await.unwrap();
        store
            .stage(
                &run,
                &StageWrite::now(
                    henk_store::Stage::Lanes,
                    henk_store::StageState::Done,
                    "1 of 1 finished",
                ),
            )
            .await
            .unwrap();
        store
            .finish_lane(&run, "lane-a", LaneStatus::Finished, 3, 10, 2, None)
            .await
            .unwrap();
        store
            .finish_run(&run, RunStatus::Finished, Some("Not bad."), None)
            .await
            .unwrap();
        store.heartbeat(&run).await.unwrap();

        let all = feed.since(feed.epoch(), 0, &run).unwrap();
        assert_eq!(
            kinds(&all),
            [
                "run",
                "lanes",
                "tool_call",
                "draft",
                "draft",
                "finding",
                "event",
                "stages",
                "lanes",
                "run",
                "heartbeat"
            ],
        );
        assert_eq!(
            all.iter().map(|c| c.seq).collect::<Vec<_>>(),
            (1..=11).collect::<Vec<_>>()
        );
        let ChangeKind::Draft(decided) = &all[4].kind else {
            panic!()
        };
        assert_eq!(
            decided.decision.as_ref().unwrap().verdict,
            DraftVerdict::Confirmed
        );
        let ChangeKind::Stages(stages) = &all[7].kind else {
            panic!()
        };
        assert_eq!(
            stages[0].detail, "1 of 1 finished",
            "read back as the store holds it"
        );
        let ChangeKind::Lanes(lanes) = &all[8].kind else {
            panic!()
        };
        assert_eq!(lanes[0].turns, 3, "read back as the store holds it");
        let ChangeKind::ToolCall(call) = &all[2].kind else {
            panic!()
        };
        assert!(!call.at.is_empty(), "a time the store chose is filled in");
        let ChangeKind::Run(ended) = &all[9].kind else {
            panic!()
        };
        assert_eq!(ended.summary.as_deref(), Some("Not bad."));
        let ChangeKind::Heartbeat(at) = &all[10].kind else {
            panic!()
        };
        assert!(!at.is_empty(), "a heartbeat says when");
    }

    #[tokio::test]
    async fn a_failed_write_announces_nothing() {
        let (store, feed) = announcing();
        store.create_run(&new_run("r-1")).await.unwrap();
        assert!(store.create_run(&new_run("r-1")).await.is_err());
        assert_eq!(feed.last(), 1);
    }

    #[test]
    fn since_replays_one_runs_missed_changes_or_says_it_cannot() {
        let feed = Feed::default();
        let (a, b) = (id("r-a"), id("r-b"));
        for n in 0..6 {
            let run = if n % 2 == 0 { &a } else { &b };
            feed.announce(run, ChangeKind::Lanes(Vec::new()));
        }
        let seqs = |changes: Vec<Arc<Change>>| changes.iter().map(|c| c.seq).collect::<Vec<_>>();
        assert_eq!(seqs(feed.since(feed.epoch(), 2, &a).unwrap()), [3, 5]);
        assert_eq!(
            seqs(feed.since(feed.epoch(), 6, &a).unwrap()),
            Vec::<u64>::new()
        );
        assert!(
            feed.since("another-epoch", 2, &a).is_none(),
            "another process"
        );
        assert!(
            feed.since(feed.epoch(), 9, &a).is_none(),
            "ahead of this feed"
        );
    }

    #[test]
    fn the_replay_is_bounded_and_says_when_it_no_longer_reaches_back() {
        let feed = Feed::default();
        let run = id("r-a");
        for _ in 0..REPLAY + 10 {
            feed.announce(&run, ChangeKind::Lanes(Vec::new()));
        }
        assert_eq!(feed.0.recent.lock().unwrap().changes.len(), REPLAY);
        assert!(
            feed.since(feed.epoch(), 5, &run).is_none(),
            "fell out of the buffer"
        );
        assert_eq!(feed.since(feed.epoch(), 10, &run).unwrap().len(), REPLAY);
    }

    #[tokio::test]
    async fn a_follower_that_falls_behind_is_dropped_and_never_holds_up_a_write() {
        let feed = Feed::default();
        let mut slow = feed.subscribe();
        let run = id("r-a");
        for _ in 0..CHANNEL + 5 {
            feed.announce(&run, ChangeKind::Lanes(Vec::new()));
        }
        assert!(matches!(
            slow.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }
}
