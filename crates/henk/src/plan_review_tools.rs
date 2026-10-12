//! The tools of a planning pairing's handoff (#328): the reviewer raises
//! each point against the drafted plan and finishes its round, and the
//! planner lists the round's points and answers each one. Nothing here
//! reaches the tracker; every point and answer goes on the run's drafts,
//! and the pairing passes them on to the other side.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{Continuation, EndReason, Ending, Tool, ToolOutput};
use henk_domain::plan_review::{PlanPoint, PointAnswer, PointId, PointLedger, Raised};
use henk_domain::run::RunId;
use henk_llm::{ToolDef, ToolName};
use henk_session::ContinuationFactory;
use henk_store::{DraftDecision, DraftRecord, DraftVerdict, PLAN_POINT_KIND, RunStore};
use serde_json::{Value, json};
use tracing::warn;

/// Nudges a session gets per round before what it left undone is settled
/// for it.
const NUDGES: u32 = 2;

/// What the pairing's tools share.
pub struct PlanHandoff {
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
    /// A handoff with no points yet.
    #[must_use]
    pub fn new(run: RunId, store: Arc<dyn RunStore>, reviewer_model: String) -> Self {
        Self {
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
                PointAnswer::Taken { .. } => (DraftVerdict::Taken, "planner"),
                PointAnswer::Rejected { .. } => (DraftVerdict::Rejected, "planner"),
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
        ToolDef {
            name: name("raise_point"),
            description: "Raises one point against the drafted plan with the planner: one real problem, such as scope, a missing step, wrong sizing, or a step that does not hold against the code or the tracker. Nothing is posted. To contest the planner's rejection of a point, once, raise it again with new evidence and name it in reopens.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "about": {"type": "string", "description": "The part of the draft it is about: a step, a sub-issue, a label, the title"},
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
            kind: PLAN_POINT_KIND.to_owned(),
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
            description: "Ends your round once every point is raised: the planner gets them next. With no new points this round, the plan is published.".to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        let Ok(mut state) = self.0.state.lock() else {
            return ToolOutput::error("points unavailable");
        };
        let round = state.ledger.round();
        let new = state.ledger.raised_in(round).count();
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
            description: "Answers one of the reviewer's points of this round, once. taken: you changed the draft so it holds no more; say what you changed, and change it with the drafting tools. rejected: it does not hold; say why, naming what shows it.".to_owned(),
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
