//! The tools of a pairing's handoff (#328): the reviewer raises each point
//! against the drafted plan and finishes its round, and the planner lists
//! the round's points and answers each one. An execute run's requirements
//! review (#535) uses the same tools against the executor's changes, with
//! the executor answering. Nothing here reaches the tracker; every point
//! and answer goes on the run's drafts, and the pairing passes them on to
//! the other side.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{Continuation, EndReason, Ending, Tool, ToolOutput};
use henk_domain::plan_review::{PlanPoint, PointAnswer, PointId, PointLedger, Raised};
use henk_domain::run::RunId;
use henk_llm::{ToolDef, ToolName};
use henk_session::ContinuationFactory;
use henk_store::{
    DraftDecision, DraftRecord, DraftVerdict, PLAN_POINT_KIND, REQUIREMENT_POINT_KIND, RunStore,
};
use serde_json::{Value, json};
use tracing::warn;

/// Nudges a session gets per round before what it left undone is settled
/// for it.
const NUDGES: u32 = 2;

/// What a pairing's reviewer raises its points against, and so who answers
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    /// A drafted plan; the planner answers (#328).
    Plan,
    /// The changes of an execute run; the executor answers (#535).
    Changes,
}

impl Subject {
    /// The `kind` the points have in the drafts table.
    fn kind(self) -> &'static str {
        match self {
            Self::Plan => PLAN_POINT_KIND,
            Self::Changes => REQUIREMENT_POINT_KIND,
        }
    }

    /// The role that answers the points, as the drafts name it.
    fn answerer(self) -> &'static str {
        match self {
            Self::Plan => "planner",
            Self::Changes => "executor",
        }
    }
}

/// What the pairing's tools share.
pub struct PlanHandoff {
    /// What the points are about.
    pub subject: Subject,
    /// The run, for its drafts.
    pub run: RunId,
    /// Run records.
    pub store: Arc<dyn RunStore>,
    /// The reviewer's model, for its drafts.
    pub reviewer_model: String,
    /// Every point and its answer.
    pub state: Mutex<PlanHandoffState>,
}

/// The points, and whether the reviewer finished the round under way.
#[derive(Debug, Default)]
pub struct PlanHandoffState {
    /// Every point of the run.
    pub ledger: PointLedger,
    /// Whether the reviewer called `finish_round` in the round under way.
    pub finished: bool,
}

impl PlanHandoff {
    /// A handoff for a drafted plan with no points yet.
    #[must_use]
    pub fn new(run: RunId, store: Arc<dyn RunStore>, reviewer_model: String) -> Self {
        Self::about(Subject::Plan, run, store, reviewer_model)
    }

    /// A handoff for an execute run's changes with no points yet (#535).
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the requirements review starts it in a later pull request (#535)"
        )
    )]
    pub fn for_changes(run: RunId, store: Arc<dyn RunStore>, reviewer_model: String) -> Self {
        Self::about(Subject::Changes, run, store, reviewer_model)
    }

    fn about(
        subject: Subject,
        run: RunId,
        store: Arc<dyn RunStore>,
        reviewer_model: String,
    ) -> Self {
        Self {
            subject,
            run,
            store,
            reviewer_model,
            state: Mutex::new(PlanHandoffState::default()),
        }
    }

    /// Starts round `round`: nothing raised yet, not finished.
    pub fn start_round(&self, round: u32) {
        if let Ok(mut state) = self.state.lock() {
            state.ledger.start_round(round);
            state.finished = false;
        }
    }

    /// Whether the reviewer called `finish_round` in the round under way.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.state.lock().is_ok_and(|s| s.finished)
    }

    /// A copy of the ledger as it stands.
    #[must_use]
    pub fn ledger(&self) -> PointLedger {
        self.state
            .lock()
            .map(|s| s.ledger.clone())
            .unwrap_or_default()
    }

    /// Every point without an answer becomes unsettled with `reason`, on
    /// the run's drafts too.
    pub async fn close_all(&self, reason: &str) {
        let closed = self
            .state
            .lock()
            .map(|mut s| s.ledger.close_all(reason))
            .unwrap_or_default();
        self.record_answers(&closed).await;
    }

    /// Records the answers of `ids` on the run's drafts.
    pub async fn record_answers(&self, ids: &[PointId]) {
        let ledger = self.ledger();
        for id in ids {
            let Some(answer) = ledger.get(*id).and_then(|p| p.answer.as_ref()) else {
                continue;
            };
            let (verdict, checker) = match answer {
                PointAnswer::Taken { .. } => (DraftVerdict::Taken, self.subject.answerer()),
                PointAnswer::Rejected { .. } => (DraftVerdict::Rejected, self.subject.answerer()),
                PointAnswer::Unsettled { .. } => (DraftVerdict::Unsettled, ""),
            };
            let decision = DraftDecision {
                at: String::new(),
                verdict,
                checker: checker.to_owned(),
                reason: answer.text().to_owned(),
                same_as: String::new(),
                comment_id: String::new(),
            };
            if let Err(error) = self
                .store
                .decide_draft(&self.run, &id.to_string(), &decision)
                .await
            {
                warn!(%error, point = %id, "could not record an answer");
            }
        }
    }
}

/// The reviewer's nudge: finish the round, at most [`NUDGES`] times.
#[must_use]
pub fn reviewer_continuation(handoff: Arc<PlanHandoff>) -> ContinuationFactory {
    Arc::new(move || {
        let handoff = Arc::clone(&handoff);
        let nudged = AtomicU32::new(0);
        let continuation: Continuation = Box::new(move |ending: &Ending<'_>| {
            if handoff.finished() || nudged.fetch_add(1, Ordering::SeqCst) >= NUDGES {
                return None;
            }
            Some(match ending.reason {
                EndReason::OutputCap => "Your answer was cut off. Raise each point with raise_point, one call per point, then call finish_round.".to_owned(),
                EndReason::EndTurn => "Raise each point with raise_point, then call finish_round. Only then end your turn.".to_owned(),
            })
        });
        continuation
    })
}

/// The planner's nudge: an answer for every point of the round, at most
/// [`NUDGES`] times.
#[must_use]
pub fn planner_continuation(handoff: Arc<PlanHandoff>) -> ContinuationFactory {
    Arc::new(move || {
        let handoff = Arc::clone(&handoff);
        let nudged = AtomicU32::new(0);
        let continuation: Continuation = Box::new(move |_: &Ending<'_>| {
            let open: Vec<String> = handoff
                .state
                .lock()
                .map(|s| s.ledger.open().iter().map(|p| p.id.to_string()).collect())
                .unwrap_or_default();
            if open.is_empty() || nudged.fetch_add(1, Ordering::SeqCst) >= NUDGES {
                return None;
            }
            Some(format!(
                "These points have no answer yet: {}. Answer each one with answer_point before you end your turn.",
                open.join(", ")
            ))
        });
        continuation
    })
}

/// `raise_point`: one point against the plan, to the planner.
pub struct RaisePoint(pub Arc<PlanHandoff>);

#[async_trait::async_trait]
impl Tool for RaisePoint {
    fn definition(&self) -> ToolDef {
        let about = match self.0.subject {
            Subject::Plan => {
                "The part of the draft it is about: a step, a sub-issue, a label, the title"
            }
            Subject::Changes => {
                "What it is about: an acceptance criterion, a file, a step of the plan"
            }
        };
        ToolDef {
            name: name("raise_point"),
            description: match self.0.subject {
                Subject::Plan => "Raises one point against the drafted plan with the planner: one real problem, such as scope, a missing step, wrong sizing, or a step that does not hold against the code or the tracker. Nothing is posted. To contest the planner's rejection of a point, once, raise it again with new evidence and name it in reopens.",
                Subject::Changes => "Raises one point against the executor's changes with the executor: one real gap between the changes and what the issue and its plan ask, such as an acceptance criterion that is not met, work outside the scope, or something missing. Nothing is posted. To contest the executor's rejection of a point, once, raise it again with new evidence and name it in reopens.",
            }
            .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "about": {"type": "string", "description": about},
                    "claim": {"type": "string", "description": "What is wrong"},
                    "why": {"type": "string", "description": "Why it matters, with the evidence"},
                    "suggestion": {"type": "string", "description": "What would fix it"},
                    "reopens": {"type": "string", "description": "The rejected point this contests, such as p2"}
                },
                "required": ["about", "claim", "why"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let handoff = &self.0;
        let text = |key: &str| arg_str(&args, key).unwrap_or_default().trim().to_owned();
        let raised = Raised {
            about: text("about"),
            claim: text("claim"),
            why: text("why"),
            suggestion: text("suggestion"),
        };
        let reopens = match arg_str(&args, "reopens").map(str::trim) {
            None | Some("") => None,
            Some(id) => match PointId::parse(id) {
                Some(id) => Some(id),
                None => return ToolOutput::error(format!("{id} is not a point id")),
            },
        };
        let id = {
            let Ok(mut state) = handoff.state.lock() else {
                return ToolOutput::error("points unavailable");
            };
            if state.finished {
                return ToolOutput::error(
                    "You finished this round; raise the rest in the next one.",
                );
            }
            match state.ledger.raise(raised.clone(), reopens) {
                Ok(id) => id,
                Err(why) => return ToolOutput::error(format!("Not raised: {why}.")),
            }
        };
        let mut body = format!("{}\n\nWhy: {}", raised.claim, raised.why);
        if !raised.suggestion.is_empty() {
            let _ = write!(body, "\n\nSuggestion: {}", raised.suggestion);
        }
        let record = DraftRecord {
            at: String::new(),
            draft: id.to_string(),
            session: "reviewer".to_owned(),
            model: handoff.reviewer_model.clone(),
            kind: handoff.subject.kind().to_owned(),
            path: raised.about,
            line: 0,
            target: reopens.map(|r| r.to_string()).unwrap_or_default(),
            body,
            decision: None,
        };
        if let Err(error) = handoff.store.record_draft(&handoff.run, &record).await {
            warn!(%error, point = %id, "could not record a point");
        }
        ToolOutput::ok(format!("Raised as {id}."))
    }
}

/// `finish_round`: the reviewer is done with the plan for now.
pub struct FinishReview(pub Arc<PlanHandoff>);

#[async_trait::async_trait]
impl Tool for FinishReview {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("finish_round"),
            description: match self.0.subject {
                Subject::Plan => "Ends your round once every point is raised: the planner gets them next. With no new points this round, the plan is published.",
                Subject::Changes => "Ends your round once every point is raised: the executor gets them next. With no new points this round, the changes are accepted and the pull request is opened.",
            }
            .to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        let Ok(mut state) = self.0.state.lock() else {
            return ToolOutput::error("points unavailable");
        };
        let round = state.ledger.round();
        let new = state.ledger.raised_in(round).count();
        state.ledger.finish_round();
        state.finished = true;
        ToolOutput::ok(format!(
            "Round {round} finished with {new} new point(s). End your turn now."
        ))
    }
}

/// `list_points`: this round's points still waiting for an answer.
pub struct ListPoints(pub Arc<PlanHandoff>);

#[async_trait::async_trait]
impl Tool for ListPoints {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_points"),
            description: "Lists the reviewer's points of this round that have no answer yet."
                .to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        let ledger = self.0.ledger();
        let open = ledger.open();
        if open.is_empty() {
            return ToolOutput::ok("Every point of this round has its answer.");
        }
        let lines: Vec<String> = open.into_iter().map(PlanPoint::text).collect();
        ToolOutput::ok(lines.join("\n"))
    }
}

/// `answer_point`: the planner's one answer to one point.
pub struct AnswerPoint(pub Arc<PlanHandoff>);

#[async_trait::async_trait]
impl Tool for AnswerPoint {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("answer_point"),
            description: match self.0.subject {
                Subject::Plan => "Answers one of the reviewer's points of this round, once. taken: you changed the draft so it holds no more; say what you changed, and change it with the drafting tools. rejected: it does not hold; say why, naming what shows it.",
                Subject::Changes => "Answers one of the reviewer's points of this round, once. taken: you changed the code so the point holds no more; make the change with the edit tools first, then say what you changed. rejected: it does not hold; say why, naming what shows it.",
            }
            .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "point": {"type": "string", "description": "The point, such as p3"},
                    "answer": {"type": "string", "enum": ["taken", "rejected"]},
                    "reason": {"type": "string", "description": "What you changed, or why not"}
                },
                "required": ["point", "answer", "reason"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let handoff = &self.0;
        let Some(id) = arg_str(&args, "point").and_then(PointId::parse) else {
            return ToolOutput::error("point must be a point id such as p3");
        };
        let reason = arg_str(&args, "reason")
            .unwrap_or_default()
            .trim()
            .to_owned();
        let answer = match arg_str(&args, "answer") {
            Some("taken") => PointAnswer::Taken { how: reason },
            Some("rejected") => PointAnswer::Rejected { reason },
            _ => return ToolOutput::error("answer must be taken or rejected"),
        };
        let given = handoff
            .state
            .lock()
            .map(|mut s| s.ledger.answer(id, answer))
            .map_err(|_| ());
        match given {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return ToolOutput::error(format!("Not recorded: {why}.")),
            Err(()) => return ToolOutput::error("points unavailable"),
        }
        handoff.record_answers(&[id]).await;
        ToolOutput::ok(format!("{id}: recorded."))
    }
}

fn name(name: &str) -> ToolName {
    ToolName::parse(name).unwrap_or_else(|_| unreachable!("tool names here are constants"))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use henk_domain::allowlist::Platform;
    use henk_domain::run::RunKind;
    use henk_store::{NewRun, SqliteStore};

    use super::*;
    use crate::ids::new_run_id;

    type Make = fn(RunId, Arc<dyn RunStore>, String) -> PlanHandoff;

    /// A handoff of `make` for a run that exists.
    async fn handoff(make: Make) -> (Arc<PlanHandoff>, Arc<dyn RunStore>, RunId) {
        let store: Arc<dyn RunStore> = Arc::new(SqliteStore::in_memory().unwrap());
        let run = new_run_id();
        store
            .create_run(&NewRun {
                id: run.clone(),
                kind: RunKind::Execute,
                platform: Platform::GitHub,
                repo: "o/r".to_owned(),
                target: 1,
                commit: None,
                requester: None,
                trigger: "test".to_owned(),
                link: String::new(),
            })
            .await
            .unwrap();
        let handoff = Arc::new(make(
            run.clone(),
            Arc::clone(&store),
            "reviewer-model".to_owned(),
        ));
        (handoff, store, run)
    }

    #[tokio::test]
    async fn points_are_stored_with_the_kind_and_the_answerer_of_their_subject() {
        let cases: [(Make, &str, &str); 2] = [
            (PlanHandoff::new, PLAN_POINT_KIND, "planner"),
            (PlanHandoff::for_changes, REQUIREMENT_POINT_KIND, "executor"),
        ];
        for (make, kind, answerer) in cases {
            let (handoff, store, run) = handoff(make).await;
            handoff.start_round(1);
            let raised = RaisePoint(Arc::clone(&handoff))
                .call(json!({
                    "about": "criterion 2",
                    "claim": "the export has no header row",
                    "why": "the issue asks for one"
                }))
                .await;
            assert!(!raised.is_error, "{raised:?}");
            let answered = AnswerPoint(Arc::clone(&handoff))
                .call(json!({"point": "p1", "answer": "taken", "reason": "added the header row"}))
                .await;
            assert!(!answered.is_error, "{answered:?}");

            let drafts = store.drafts(&run).await.unwrap();
            assert_eq!(drafts.len(), 1, "{kind}");
            assert_eq!(drafts[0].kind, kind);
            assert_eq!(drafts[0].session, "reviewer");
            assert_eq!(drafts[0].model, "reviewer-model");
            let decision = drafts[0].decision.as_ref().unwrap();
            assert_eq!(decision.verdict, DraftVerdict::Taken, "{kind}");
            assert_eq!(decision.checker, answerer, "{kind}");
        }
    }

    #[tokio::test]
    async fn the_tools_speak_of_the_plan_or_of_the_changes() {
        let (plan, _, _) = handoff(PlanHandoff::new).await;
        let (changes, _, _) = handoff(PlanHandoff::for_changes).await;
        let described = |handoff: &Arc<PlanHandoff>| {
            let about = RaisePoint(Arc::clone(handoff)).definition();
            (
                about.description,
                about.input_schema["properties"]["about"]["description"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                FinishReview(Arc::clone(handoff)).definition().description,
                AnswerPoint(Arc::clone(handoff)).definition().description,
            )
        };
        let (raise, about, finish, answer) = described(&plan);
        assert!(raise.contains("drafted plan"), "{raise}");
        assert!(about.contains("sub-issue"), "{about}");
        assert!(finish.contains("plan is published"), "{finish}");
        assert!(answer.contains("drafting tools"), "{answer}");

        let (raise, about, finish, answer) = described(&changes);
        assert!(raise.contains("executor's changes"), "{raise}");
        assert!(about.contains("acceptance criterion"), "{about}");
        assert!(finish.contains("pull request is opened"), "{finish}");
        assert!(answer.contains("edit tools"), "{answer}");
        // Everything a model reads follows the style rules (no emoji, no
        // em-dash).
        for text in [raise, about, finish, answer] {
            assert!(
                henk_domain::text::style_violations(&text).is_empty(),
                "{text}"
            );
        }
    }
}
