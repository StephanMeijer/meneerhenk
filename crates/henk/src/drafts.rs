//! What becomes of a review's drafts once the lanes have ended (#189):
//! the fact-check's verdicts are settled ([`henk_domain::draft::settle`])
//! and Henk's code writes what holds. A model never writes to the platform
//! (§8.4): lanes only queue drafts, and this module posts, rewrites and
//! withdraws, with the checking model on each comment's marker (§8.6).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use henk_domain::draft::{Draft, DraftBook, DraftId, DraftKind, Original, Settled, Verdict};
use henk_domain::finding::{Finding, FindingRegistry};
use henk_domain::marker::{Marker, MarkerKind, ModelId, Withdrawal};
use henk_domain::review::CommitSha;
use henk_domain::run::RunId;
use henk_platform::{PlatformWriter, ReviewTarget};
use henk_store::{DraftDecision, DraftVerdict, FindingAction, RunStore};
use tracing::{info, warn};

use crate::review_tools::visible_text;

/// What writing a review's drafts needs.
pub struct ReviewWrites {
    /// The run.
    pub run: RunId,
    /// The pull/merge request.
    pub target: ReviewTarget,
    /// The reviewed commit.
    pub commit: CommitSha,
    /// Where comments go.
    pub writer: Arc<dyn PlatformWriter>,
    /// Run records.
    pub store: Arc<dyn RunStore>,
    /// The findings on the target, kept up to date with what is written.
    pub registry: Arc<Mutex<FindingRegistry>>,
}

/// Records every draft as cancelled: the review ended before they were
/// settled, and nothing is written.
pub async fn cancel_all(writes: &ReviewWrites, book: &DraftBook) {
    for draft in book.iter() {
        decide(
            writes,
            draft,
            DraftVerdict::Cancelled,
            None,
            "the review was cancelled before the fact-check",
            "",
            "",
        )
        .await;
    }
}

/// Settles `verdicts` and writes what holds, in draft order, recording
/// what became of each draft on the run.
pub async fn write_all(
    writes: &ReviewWrites,
    book: &DraftBook,
    verdicts: &BTreeMap<DraftId, Verdict>,
) {
    let mut written: BTreeMap<DraftId, String> = BTreeMap::new();
    for settled in henk_domain::draft::settle(book, verdicts) {
        let Some(draft) = book.get(settled.id()) else {
            continue;
        };
        let reason = match verdicts.get(&draft.id) {
            Some(
                Verdict::Confirmed { reason, .. }
                | Verdict::Rejected { reason, .. }
                | Verdict::SameAs { reason, .. },
            ) => reason.clone(),
            Some(Verdict::Unchecked { why }) => why.clone(),
            Some(Verdict::NoCheck) | None => String::new(),
        };
        match settled {
            Settled::Publish {
                checked_by,
                unchecked,
                ..
            } => {
                if let Some(comment) =
                    write_one(writes, draft, checked_by, unchecked.as_deref(), &reason).await
                {
                    written.insert(draft.id, comment);
                }
            }
            Settled::Reject { by, reason, .. } => {
                note(
                    writes,
                    "info",
                    &format!(
                        "{}: {} {} on {}:{} rejected by {by}: {reason}",
                        draft.lane,
                        draft.id,
                        draft.kind.as_str(),
                        draft.key.path,
                        draft.key.line
                    ),
                )
                .await;
                let target = draft.kind.comment_id().unwrap_or_default().to_owned();
                record(writes, draft, &target, FindingAction::Rejected).await;
                decide(
                    writes,
                    draft,
                    DraftVerdict::Rejected,
                    Some(&by),
                    &reason,
                    "",
                    "",
                )
                .await;
            }
            Settled::Merge { by, into, .. } => {
                let comment = match &into {
                    Original::Draft(id) => written.get(id).cloned().unwrap_or_default(),
                    Original::Comment(comment) => comment.clone(),
                };
                note(
                    writes,
                    "info",
                    &format!(
                        "{}: {} on {}:{} repeats {into} ({by}), merged into it",
                        draft.lane, draft.id, draft.key.path, draft.key.line
                    ),
                )
                .await;
                record(writes, draft, &comment, FindingAction::Merged).await;
                let same_as = match &into {
                    Original::Draft(id) => id.to_string(),
                    Original::Comment(comment) => comment.clone(),
                };
                decide(
                    writes,
                    draft,
                    DraftVerdict::SameAs,
                    Some(&by),
                    &reason,
                    &same_as,
                    &comment,
                )
                .await;
            }
        }
    }
}

/// Writes a draft that holds, or that went unchecked, and records what
/// became of it. Returns its comment when it was written.
async fn write_one(
    writes: &ReviewWrites,
    draft: &Draft,
    checked_by: Option<ModelId>,
    unchecked: Option<&str>,
    reason: &str,
) -> Option<String> {
    let verdict = match (&checked_by, unchecked) {
        (Some(_), _) => DraftVerdict::Confirmed,
        (None, Some(_)) => DraftVerdict::Unchecked,
        (None, None) => DraftVerdict::NotChecked,
    };
    timeline(writes, draft, checked_by.as_ref(), unchecked, reason).await;
    match publish(writes, draft, checked_by.clone()).await {
        Ok(comment) => {
            if unchecked.is_some() {
                record(writes, draft, &comment, FindingAction::Unverified).await;
            }
            decide(
                writes,
                draft,
                verdict,
                checked_by.as_ref(),
                reason,
                "",
                &comment,
            )
            .await;
            Some(comment)
        }
        Err(error) => {
            decide(
                writes,
                draft,
                DraftVerdict::Failed,
                checked_by.as_ref(),
                &error,
                "",
                "",
            )
            .await;
            None
        }
    }
}

/// The timeline line for a draft about to be written.
async fn timeline(
    writes: &ReviewWrites,
    draft: &Draft,
    checked_by: Option<&ModelId>,
    unchecked: Option<&str>,
    reason: &str,
) {
    let place = format!(
        "{}: {} {} on {}:{}",
        draft.lane,
        draft.id,
        draft.kind.as_str(),
        draft.key.path,
        draft.key.line
    );
    match (checked_by, unchecked) {
        (Some(by), _) => {
            note(
                writes,
                "info",
                &format!("{place} confirmed by {by}: {reason}"),
            )
            .await;
        }
        (None, Some(why)) => {
            note(
                writes,
                "warn",
                &format!("{place} went out unchecked: {why}"),
            )
            .await;
        }
        (None, None) => {}
    }
}

/// Writes one draft and returns its comment's id, or why it failed, which
/// is also on the timeline.
async fn publish(
    writes: &ReviewWrites,
    draft: &Draft,
    checked_by: Option<ModelId>,
) -> Result<String, String> {
    let marker = Marker {
        run: writes.run.clone(),
        model: draft.model.clone(),
        requested_by: None,
        kind: Some(MarkerKind::Finding),
        checked_by: checked_by.clone(),
        withdrawn: None,
    };
    let (path, line) = (draft.key.path.as_str(), draft.key.line);
    let written = match &draft.kind {
        DraftKind::Finding { side } => {
            let body = marker.attach(&draft.text);
            writes
                .writer
                .post_finding(&writes.target, &writes.commit, path, line, *side, &body)
                .await
                .map(|posted| {
                    if let Ok(mut registry) = writes.registry.lock() {
                        registry.record(Finding {
                            key: draft.key.clone(),
                            comment_id: posted.id.clone(),
                            body,
                            lane: Some(draft.lane.clone()),
                            answered_by_person: false,
                            resolved: false,
                            in_diff: true,
                        });
                    }
                    (posted.id, FindingAction::Posted)
                })
        }
        DraftKind::Rewrite { comment_id, .. } => {
            let body = marker.attach(&draft.text);
            writes
                .writer
                .update_finding(&writes.target, comment_id, &body)
                .await
                .map(|()| {
                    if let Ok(mut registry) = writes.registry.lock() {
                        registry.improve(&draft.key, body, Some(draft.lane.clone()));
                    }
                    (comment_id.clone(), FindingAction::Improved)
                })
        }
        DraftKind::Withdrawal { comment_id, .. } => {
            withdraw(writes, draft, comment_id, marker, checked_by)
                .await
                .map(|()| (comment_id.clone(), FindingAction::Withdrawn))
        }
    };
    match written {
        Ok((comment, action)) => {
            record(writes, draft, &comment, action).await;
            info!(lane = %draft.lane, draft = %draft.id, path, line, %comment, "draft written");
            Ok(comment)
        }
        Err(error) => {
            warn!(%error, draft = %draft.id, path, line, "writing a draft failed");
            let text = format!("could not write it: {error}");
            note(
                writes,
                "warn",
                &format!(
                    "{}: {} {} on {path}:{line} {text}",
                    draft.lane,
                    draft.id,
                    draft.kind.as_str()
                ),
            )
            .await;
            Err(text)
        }
    }
}

/// Replaces a finding's text with the reason it is withdrawn and resolves
/// its thread. The finding keeps who wrote it; the withdrawal is added
/// (§8.6).
async fn withdraw(
    writes: &ReviewWrites,
    draft: &Draft,
    comment_id: &str,
    fallback: Marker,
    checked_by: Option<ModelId>,
) -> Result<(), henk_platform::PlatformError> {
    let current = writes
        .registry
        .lock()
        .ok()
        .and_then(|r| r.get(&draft.key).map(|f| f.body.clone()))
        .unwrap_or_default();
    let original = Marker::parse(&current).unwrap_or(fallback);
    let body = Marker {
        withdrawn: Some(Withdrawal {
            run: writes.run.clone(),
            model: draft.model.clone(),
            checked_by,
        }),
        ..original
    }
    .attach(&format!("Withdrawn. {}", draft.text));
    writes
        .writer
        .update_finding(&writes.target, comment_id, &body)
        .await?;
    if let Err(error) = writes
        .writer
        .resolve_finding(&writes.target, comment_id)
        .await
    {
        // The text already says it is withdrawn; an open thread only
        // means it still counts until someone resolves it.
        warn!(%error, comment = comment_id, "could not resolve a withdrawn finding");
        note(
            writes,
            "warn",
            &format!(
                "{}: withdrew {comment_id} but could not resolve its thread: {error}",
                draft.lane
            ),
        )
        .await;
    }
    if let Ok(mut registry) = writes.registry.lock() {
        registry.withdraw(&draft.key, body);
    }
    Ok(())
}

async fn record(writes: &ReviewWrites, draft: &Draft, comment: &str, action: FindingAction) {
    let _ = writes
        .store
        .record_finding(
            &writes.run,
            draft.lane.as_str(),
            &draft.key.path,
            draft.key.line,
            comment,
            action,
        )
        .await;
}

async fn decide(
    writes: &ReviewWrites,
    draft: &Draft,
    verdict: DraftVerdict,
    checker: Option<&ModelId>,
    reason: &str,
    same_as: &str,
    comment: &str,
) {
    let decision = DraftDecision {
        at: String::new(),
        verdict,
        checker: checker.map(ToString::to_string).unwrap_or_default(),
        reason: reason.to_owned(),
        same_as: same_as.to_owned(),
        comment_id: comment.to_owned(),
    };
    if let Err(error) = writes
        .store
        .decide_draft(&writes.run, &draft.id.to_string(), &decision)
        .await
    {
        warn!(%error, draft = %draft.id, "could not record what became of a draft");
    }
}

async fn note(writes: &ReviewWrites, level: &str, text: &str) {
    if let Err(error) = writes.store.event(&writes.run, level, text).await {
        warn!(%error, "could not record a draft's outcome on the timeline");
    }
}

/// The findings a draft may repeat: the ones on the target that are not
/// resolved, with their visible text.
#[must_use]
pub fn open_findings(registry: &Mutex<FindingRegistry>) -> Vec<Finding> {
    registry
        .lock()
        .map(|r| {
            r.iter()
                .filter(|f| !f.resolved)
                .map(|f| Finding {
                    body: visible_text(&f.body).to_owned(),
                    ..f.clone()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::diff::DiffSide;
    use henk_domain::finding::FindingKey;
    use henk_domain::review::LaneName;
    use henk_store::{DraftRecord, SqliteStore};

    use super::*;
    use crate::listeners::testing::FakeWriter;

    struct Setup {
        writes: ReviewWrites,
        writer: Arc<FakeWriter>,
        book: DraftBook,
    }

    async fn setup() -> Setup {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let run = RunId::parse("r-1").unwrap();
        store
            .create_run(&henk_store::NewRun {
                id: run.clone(),
                kind: henk_domain::run::RunKind::Review,
                platform: Platform::GitHub,
                repo: "o/r".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "test".into(),
                link: "l".into(),
            })
            .await
            .unwrap();
        let writer = Arc::new(FakeWriter {
            accept_posts: true,
            ..FakeWriter::default()
        });
        Setup {
            writes: ReviewWrites {
                run,
                target: ReviewTarget {
                    repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                    number: 7,
                },
                commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
                writer: Arc::clone(&writer) as Arc<dyn PlatformWriter>,
                store,
                registry: Arc::new(Mutex::new(FindingRegistry::new())),
            },
            writer,
            book: DraftBook::new(),
        }
    }

    fn key(line: u32) -> FindingKey {
        FindingKey {
            path: "src/a.rs".into(),
            line,
        }
    }

    impl Setup {
        /// Queues a draft as a lane's tool does, on the run too.
        async fn draft(&mut self, lane: &str, kind: DraftKind, row: u32, text: &str) -> DraftId {
            let model = ModelId::parse(if lane == "lane-a" { "m" } else { "n" }).unwrap();
            let id = self
                .book
                .add(&LaneName::new(lane), &model, kind.clone(), key(row), text)
                .unwrap();
            self.writes
                .store
                .record_draft(
                    &self.writes.run,
                    &DraftRecord {
                        at: String::new(),
                        draft: id.to_string(),
                        lane: lane.into(),
                        model: model.to_string(),
                        kind: kind.as_str().into(),
                        path: "src/a.rs".into(),
                        line: row,
                        target: kind.comment_id().unwrap_or_default().into(),
                        body: text.into(),
                        decision: None,
                    },
                )
                .await
                .unwrap();
            id
        }

        async fn actions(&self) -> Vec<(String, String)> {
            self.writes
                .store
                .findings(&self.writes.run)
                .await
                .unwrap()
                .into_iter()
                .map(|f| (f.action, f.comment_id))
                .collect()
        }

        async fn decisions(&self) -> Vec<DraftDecision> {
            self.writes
                .store
                .drafts(&self.writes.run)
                .await
                .unwrap()
                .into_iter()
                .map(|d| d.decision.unwrap())
                .collect()
        }
    }

    fn finding() -> DraftKind {
        DraftKind::Finding {
            side: DiffSide::Right,
        }
    }

    fn by() -> ModelId {
        ModelId::parse("opus").unwrap()
    }

    #[tokio::test]
    async fn confirmed_drafts_are_posted_rejected_ones_not_and_repeats_merged() {
        let mut s = setup().await;
        let first = s.draft("lane-a", finding(), 2, "x is never set.").await;
        let wrong = s.draft("lane-b", finding(), 3, "y is wrong.").await;
        let repeat = s.draft("lane-b", finding(), 4, "x is not set.").await;
        let verdicts = BTreeMap::from([
            (
                first,
                Verdict::Confirmed {
                    by: by(),
                    reason: "Line 2 never assigns x.".into(),
                },
            ),
            (
                wrong,
                Verdict::Rejected {
                    by: by(),
                    reason: "src/a.rs:3 is fine.".into(),
                },
            ),
            (
                repeat,
                Verdict::SameAs {
                    by: by(),
                    of: Original::Draft(first),
                    reason: "Same as d1.".into(),
                },
            ),
        ]);
        write_all(&s.writes, &s.book, &verdicts).await;

        let posts = s.writer.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1, "one problem, one comment");
        let marker = Marker::parse(&posts[0].2).unwrap();
        assert_eq!(marker.model.as_str(), "m", "the lane's model wrote it");
        assert_eq!(marker.checked_by.unwrap().as_str(), "opus");
        assert!(posts[0].2.starts_with("x is never set."));
        assert_eq!(
            s.actions().await,
            [
                ("posted".to_owned(), "c1".to_owned()),
                ("rejected".to_owned(), String::new()),
                ("merged".to_owned(), "c1".to_owned()),
            ]
        );
        let decisions = s.decisions().await;
        assert_eq!(decisions[0].verdict, DraftVerdict::Confirmed);
        assert_eq!(decisions[0].comment_id, "c1");
        assert_eq!(decisions[0].checker, "opus");
        assert_eq!(decisions[1].verdict, DraftVerdict::Rejected);
        assert_eq!(decisions[1].reason, "src/a.rs:3 is fine.");
        assert_eq!(
            (
                decisions[2].verdict,
                decisions[2].same_as.as_str(),
                decisions[2].comment_id.as_str()
            ),
            (DraftVerdict::SameAs, "d1", "c1")
        );
        assert_eq!(s.writes.registry.lock().unwrap().open_count(), 1);
        let events = s.writes.store.events(&s.writes.run).await.unwrap();
        assert!(
            events.iter().any(|e| e.message
                == "lane-b: d2 finding on src/a.rs:3 rejected by opus: src/a.rs:3 is fine."),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn an_unchecked_draft_goes_out_and_says_so_and_without_a_checker_it_just_goes_out() {
        let mut s = setup().await;
        let unchecked = s.draft("lane-a", finding(), 2, "x is never set.").await;
        let plain = s.draft("lane-a", finding(), 3, "y leaks.").await;
        let verdicts = BTreeMap::from([
            (
                unchecked,
                Verdict::Unchecked {
                    why: "no verdict from opus".into(),
                },
            ),
            (plain, Verdict::NoCheck),
        ]);
        write_all(&s.writes, &s.book, &verdicts).await;
        assert_eq!(
            s.actions().await,
            [
                ("posted".to_owned(), "c1".to_owned()),
                ("unverified".to_owned(), "c1".to_owned()),
                ("posted".to_owned(), "c2".to_owned()),
            ]
        );
        let decisions = s.decisions().await;
        assert_eq!(decisions[0].verdict, DraftVerdict::Unchecked);
        assert_eq!(decisions[0].reason, "no verdict from opus");
        assert_eq!(decisions[1].verdict, DraftVerdict::NotChecked);
        let posts = s.writer.posts.lock().unwrap();
        assert!(
            posts
                .iter()
                .all(|p| Marker::parse(&p.2).unwrap().checked_by.is_none())
        );
    }

    #[tokio::test]
    async fn a_withdrawal_keeps_the_findings_author_and_names_who_withdrew_it() {
        let mut s = setup().await;
        // A finding an earlier run posted, with another model.
        let original = Marker {
            run: RunId::parse("r-0").unwrap(),
            model: ModelId::parse("orig").unwrap(),
            checked_by: None,
            requested_by: None,
            kind: Some(MarkerKind::Finding),
            withdrawn: None,
        };
        s.writes.registry.lock().unwrap().record(Finding {
            key: key(2),
            comment_id: "c9".into(),
            body: original.attach("x is never set."),
            lane: None,
            answered_by_person: false,
            resolved: false,
            in_diff: true,
        });
        let id = s
            .draft(
                "lane-a",
                DraftKind::Withdrawal {
                    comment_id: "c9".into(),
                    finding: "x is never set.".into(),
                },
                2,
                "src/a.rs:2 sets x.",
            )
            .await;
        let verdicts = BTreeMap::from([(
            id,
            Verdict::Confirmed {
                by: by(),
                reason: "It does.".into(),
            },
        )]);
        write_all(&s.writes, &s.book, &verdicts).await;

        let updates = s.writer.updates.lock().unwrap().clone();
        let body = &updates[0].1;
        let marker = Marker::parse(body).unwrap();
        assert_eq!(marker.run.as_str(), "r-0", "the original run stays");
        assert_eq!(marker.model.as_str(), "orig", "the original model stays");
        let withdrawal = marker.withdrawn.unwrap();
        assert_eq!(withdrawal.run, s.writes.run);
        assert_eq!(withdrawal.model.as_str(), "m");
        assert_eq!(withdrawal.checked_by.unwrap().as_str(), "opus");
        assert!(body.starts_with("Withdrawn. src/a.rs:2 sets x."), "{body}");
        assert_eq!(*s.writer.resolved.lock().unwrap(), ["c9"]);
        assert_eq!(s.writes.registry.lock().unwrap().open_count(), 0);
        assert_eq!(
            s.actions().await,
            [("withdrawn".to_owned(), "c9".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_confirmed_rewrite_replaces_the_text_and_a_failed_post_is_recorded() {
        let mut s = setup().await;
        s.writes.registry.lock().unwrap().record(Finding {
            key: key(2),
            comment_id: "c9".into(),
            body: "x is never set.".into(),
            lane: None,
            answered_by_person: false,
            resolved: false,
            in_diff: true,
        });
        let rewrite = s
            .draft(
                "lane-b",
                DraftKind::Rewrite {
                    comment_id: "c9".into(),
                    current: "x is never set.".into(),
                },
                2,
                "x is never set on the error path.",
            )
            .await;
        let confirmed = |id| {
            (
                id,
                Verdict::Confirmed {
                    by: by(),
                    reason: "Holds.".into(),
                },
            )
        };
        write_all(&s.writes, &s.book, &BTreeMap::from([confirmed(rewrite)])).await;
        let updates = s.writer.updates.lock().unwrap().clone();
        assert_eq!(updates[0].0, "c9");
        assert!(
            updates[0]
                .1
                .starts_with("x is never set on the error path.")
        );
        assert_eq!(
            s.actions().await,
            [("improved".to_owned(), "c9".to_owned())]
        );

        let mut refused = setup().await;
        let writer = Arc::new(FakeWriter::default());
        refused.writes.writer = Arc::clone(&writer) as Arc<dyn PlatformWriter>;
        let id = refused
            .draft("lane-a", finding(), 2, "x is never set.")
            .await;
        write_all(
            &refused.writes,
            &refused.book,
            &BTreeMap::from([confirmed(id)]),
        )
        .await;
        let decision = &refused.decisions().await[0];
        assert_eq!(decision.verdict, DraftVerdict::Failed);
        assert!(
            decision.reason.contains("not in the fake"),
            "{}",
            decision.reason
        );
    }

    #[tokio::test]
    async fn cancelled_drafts_write_nothing() {
        let mut s = setup().await;
        s.draft("lane-a", finding(), 2, "x is never set.").await;
        cancel_all(&s.writes, &s.book).await;
        assert!(s.writer.posts.lock().unwrap().is_empty());
        assert_eq!(s.decisions().await[0].verdict, DraftVerdict::Cancelled);
    }
}
