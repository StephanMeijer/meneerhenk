//! The tools of a review loop's handoff (#285): the reviewer reports each
//! finding and finishes its round, and the fixer lists the round's findings
//! and gives each one verdict. Nothing here reaches the platform; every
//! finding and verdict goes on the run's drafts, and the loop passes them
//! on to the other side.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use henk_agent::{Continuation, EndReason, Ending, Tool, ToolOutput};
use henk_domain::review_loop::{FindingId, Ledger, LoopFinding, LoopVerdict, Report};
use henk_domain::run::RunId;
use henk_llm::{ToolDef, ToolName};
use henk_session::ContinuationFactory;
use henk_store::{DraftDecision, DraftRecord, DraftVerdict, LOOP_FINDING_KIND, RunStore};
use serde_json::{Value, json};
use tracing::warn;

/// Nudges a session gets per round before what it left undone is settled
/// for it.
const NUDGES: u32 = 2;

/// What the loop's tools share.
pub struct Handoff {
    /// The run, for its drafts.
    pub run: RunId,
    /// Run records.
    pub store: Arc<dyn RunStore>,
    /// The reviewer's model, for its drafts.
    pub reviewer_model: String,
    /// Every finding and its verdict.
    pub state: Mutex<HandoffState>,
}

/// The findings, and whether the reviewer finished the round under way.
#[derive(Debug, Default)]
pub struct HandoffState {
    /// Every finding of the run.
    pub ledger: Ledger,
    /// The files the reviewer says it covered, once it finished the round.
    pub finished: Option<Vec<String>>,
}

impl Handoff {
    /// Starts round `round`: nothing reported yet, not finished.
    pub fn start_round(&self, round: u32) {
        if let Ok(mut state) = self.state.lock() {
            state.ledger.start_round(round);
            state.finished = None;
        }
    }

    /// Whether the reviewer called `finish_round` in the round under way.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.state.lock().is_ok_and(|s| s.finished.is_some())
    }

    /// A copy of the ledger as it stands.
    #[must_use]
    pub fn ledger(&self) -> Ledger {
        self.state
            .lock()
            .map(|s| s.ledger.clone())
            .unwrap_or_default()
    }

    /// Changes the ledger under its lock.
    pub fn with_ledger<T>(&self, change: impl FnOnce(&mut Ledger) -> T) -> Option<T> {
        self.state.lock().ok().map(|mut s| change(&mut s.ledger))
    }

    /// Records the verdicts of `ids` on the run's drafts.
    pub async fn record_verdicts(&self, ids: &[FindingId]) {
        let ledger = self.ledger();
        for id in ids {
            let Some(finding) = ledger.get(*id) else {
                continue;
            };
            let Some(verdict) = &finding.verdict else {
                continue;
            };
            let decision = decision(verdict);
            if let Err(error) = self
                .store
                .decide_draft(&self.run, &id.to_string(), &decision)
                .await
            {
                warn!(%error, finding = %id, "could not record a verdict");
            }
        }
    }
}

/// A verdict as the drafts table keeps it: the fixer's, or the loop's own
/// for an unsettled finding; a fix with its commit.
fn decision(verdict: &LoopVerdict) -> DraftDecision {
    let (stored, checker, commit) = match verdict {
        LoopVerdict::Fixed { commit, .. } => (
            DraftVerdict::Fixed,
            "fixer",
            commit.clone().unwrap_or_default(),
        ),
        LoopVerdict::Rejected { .. } => (DraftVerdict::Rejected, "fixer", String::new()),
        LoopVerdict::WontFix { .. } => (DraftVerdict::WontFix, "fixer", String::new()),
        LoopVerdict::Unsettled { .. } => (DraftVerdict::Unsettled, "", String::new()),
    };
    DraftDecision {
        at: String::new(),
        verdict: stored,
        checker: checker.to_owned(),
        reason: verdict.text().to_owned(),
        same_as: String::new(),
        comment_id: commit,
    }
}

/// One finding as the other side reads it.
#[must_use]
pub fn finding_text(finding: &LoopFinding) -> String {
    let report = &finding.report;
    let mut text = format!(
        "{} at {}:{}: {}\n  why: {}",
        finding.id, report.path, report.line, report.claim, report.why
    );
    if !report.fix.trim().is_empty() {
        let _ = write!(text, "\n  fix: {}", report.fix);
    }
    if let Some(reopens) = finding.reopens {
        let _ = write!(text, "\n  contests the rejection of {reopens}");
    }
    text
}

/// One finding's verdict as the reviewer reads it.
#[must_use]
pub fn verdict_text(finding: &LoopFinding) -> String {
    let report = &finding.report;
    let fate = match &finding.verdict {
        None => "no verdict".to_owned(),
        Some(LoopVerdict::Fixed { what, commit }) => match commit {
            Some(sha) => format!("fixed in {}: {what}", sha.get(..12).unwrap_or(sha)),
            None => format!("fixed: {what}"),
        },
        Some(verdict) => format!("{}: {}", verdict.as_str().replace('_', " "), verdict.text()),
    };
    format!(
        "{} at {}:{} ({}): {fate}",
        finding.id, report.path, report.line, report.claim
    )
}

/// The reviewer's nudge: finish the round, at most [`NUDGES`] times.
#[must_use]
pub fn reviewer_continuation(handoff: Arc<Handoff>) -> ContinuationFactory {
    Arc::new(move || {
        let handoff = Arc::clone(&handoff);
        let nudged = AtomicU32::new(0);
        let continuation: Continuation = Box::new(move |ending: &Ending<'_>| {
            if handoff.finished() || nudged.fetch_add(1, Ordering::SeqCst) >= NUDGES {
                return None;
            }
            Some(match ending.reason {
                EndReason::OutputCap => "Your answer was cut off. Report each finding with report_finding, one call per finding, then call finish_round.".to_owned(),
                EndReason::EndTurn => "Report each finding with report_finding, then call finish_round with the files you covered. Only then end your turn.".to_owned(),
            })
        });
        continuation
    })
}

/// The fixer's nudge: a verdict for every finding of the round, at most
/// [`NUDGES`] times.
#[must_use]
pub fn fixer_continuation(handoff: Arc<Handoff>) -> ContinuationFactory {
    Arc::new(move || {
        let handoff = Arc::clone(&handoff);
        let nudged = AtomicU32::new(0);
        let continuation: Continuation = Box::new(move |_: &Ending<'_>| {
            let open: Vec<String> = handoff
                .state
                .lock()
                .map(|s| s.ledger.open().iter().map(|f| f.id.to_string()).collect())
                .unwrap_or_default();
            if open.is_empty() || nudged.fetch_add(1, Ordering::SeqCst) >= NUDGES {
                return None;
            }
            Some(format!(
                "These findings have no verdict yet: {}. Give each one with give_verdict before you end your turn.",
                open.join(", ")
            ))
        });
        continuation
    })
}

/// `report_finding`: one finding, to the fixer.
pub struct ReportFinding(pub Arc<Handoff>);

#[async_trait::async_trait]
impl Tool for ReportFinding {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("report_finding"),
            description: "Reports one finding to the fixer: one real problem this change introduces, at one line. Nothing is posted on the pull request. To contest the fixer's rejection of a finding, once, report it again with new evidence and name it in reopens.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path as in the diff"},
                    "line": {"type": "integer", "description": "Line in the new version of the file"},
                    "claim": {"type": "string", "description": "What is wrong"},
                    "why": {"type": "string", "description": "Why it matters"},
                    "fix": {"type": "string", "description": "What would fix it"},
                    "reopens": {"type": "string", "description": "The rejected finding this contests, such as f2"}
                },
                "required": ["path", "line", "claim", "why"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let handoff = &self.0;
        let text = |key: &str| arg_str(&args, key).unwrap_or_default().trim().to_owned();
        let report = Report {
            path: text("path"),
            line: args
                .get("line")
                .and_then(Value::as_u64)
                .and_then(|l| u32::try_from(l).ok())
                .unwrap_or(0),
            claim: text("claim"),
            why: text("why"),
            fix: text("fix"),
        };
        let reopens = match arg_str(&args, "reopens").map(str::trim) {
            None | Some("") => None,
            Some(id) => match FindingId::parse(id) {
                Some(id) => Some(id),
                None => return ToolOutput::error(format!("{id} is not a finding id")),
            },
        };
        let reported = {
            let Ok(mut state) = handoff.state.lock() else {
                return ToolOutput::error("findings unavailable");
            };
            if state.finished.is_some() {
                return ToolOutput::error(
                    "You finished this round; report the rest in the next one.",
                );
            }
            state.ledger.report(report.clone(), reopens)
        };
        let id = match reported {
            Ok(id) => id,
            Err(why) => return ToolOutput::error(format!("Not reported: {why}.")),
        };
        let mut body = format!("{}\n\nWhy: {}", report.claim, report.why);
        if !report.fix.is_empty() {
            let _ = write!(body, "\n\nFix: {}", report.fix);
        }
        let record = DraftRecord {
            at: String::new(),
            draft: id.to_string(),
            lane: "reviewer".to_owned(),
            model: handoff.reviewer_model.clone(),
            kind: LOOP_FINDING_KIND.to_owned(),
            path: report.path,
            line: report.line,
            target: reopens.map(|r| r.to_string()).unwrap_or_default(),
            body,
            decision: None,
        };
        if let Err(error) = handoff.store.record_draft(&handoff.run, &record).await {
            warn!(%error, finding = %id, "could not record a finding");
        }
        ToolOutput::ok(format!("Reported as {id}."))
    }
}

/// `finish_round`: the reviewer is done for now.
pub struct FinishRound(pub Arc<Handoff>);

#[async_trait::async_trait]
impl Tool for FinishRound {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("finish_round"),
            description: "Ends your round once every finding is reported: the fixer gets them next. Lists the files you covered. With no new findings this round, the review is done.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "covered": {"type": "array", "items": {"type": "string"}, "description": "The files you reviewed this round"}
                },
                "required": ["covered"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let covered: Vec<String> = args
            .get("covered")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let Ok(mut state) = self.0.state.lock() else {
            return ToolOutput::error("findings unavailable");
        };
        let round = state.ledger.round();
        let new = state.ledger.reported_in(round).count();
        state.finished = Some(covered);
        ToolOutput::ok(format!(
            "Round {round} finished with {new} new finding(s). End your turn now."
        ))
    }
}

/// `list_findings`: this round's findings still waiting for a verdict.
pub struct ListFindings(pub Arc<Handoff>);

#[async_trait::async_trait]
impl Tool for ListFindings {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("list_findings"),
            description: "Lists this round's findings that have no verdict yet.".to_owned(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        let ledger = self.0.ledger();
        let open = ledger.open();
        if open.is_empty() {
            return ToolOutput::ok("Every finding of this round has its verdict.");
        }
        let lines: Vec<String> = open.into_iter().map(finding_text).collect();
        ToolOutput::ok(lines.join("\n"))
    }
}

/// `give_verdict`: the fixer's one verdict on one finding.
pub struct GiveVerdict(pub Arc<Handoff>);

#[async_trait::async_trait]
impl Tool for GiveVerdict {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: name("give_verdict"),
            description: "Gives one finding of this round its verdict, once. fixed: you changed the code so it holds no more; say what you changed. rejected: it does not hold; say why, naming the code that shows it. wont_fix: it holds but is not for this pull request; say why.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "finding": {"type": "string", "description": "The finding, such as f3"},
                    "verdict": {"type": "string", "enum": ["fixed", "rejected", "wont_fix"]},
                    "reason": {"type": "string", "description": "What you changed, or why not"}
                },
                "required": ["finding", "verdict", "reason"]
            }),
        }
    }

    async fn call(&self, args: Value) -> ToolOutput {
        let handoff = &self.0;
        let Some(id) = arg_str(&args, "finding").and_then(FindingId::parse) else {
            return ToolOutput::error("finding must be a finding id such as f3");
        };
        let reason = arg_str(&args, "reason")
            .unwrap_or_default()
            .trim()
            .to_owned();
        let verdict = match arg_str(&args, "verdict") {
            Some("fixed") => LoopVerdict::Fixed {
                what: reason,
                commit: None,
            },
            Some("rejected") => LoopVerdict::Rejected { reason },
            Some("wont_fix") => LoopVerdict::WontFix { reason },
            _ => return ToolOutput::error("verdict must be fixed, rejected or wont_fix"),
        };
        let fixed = matches!(verdict, LoopVerdict::Fixed { .. });
        if let Some(Err(why)) = handoff.with_ledger(|ledger| ledger.give(id, verdict)) {
            return ToolOutput::error(format!("Not recorded: {why}."));
        }
        if fixed {
            // A fix holds once Henk pushed it, after this round.
            return ToolOutput::ok(format!(
                "{id}: fixed, once your changes are pushed after this round."
            ));
        }
        handoff.record_verdicts(&[id]).await;
        ToolOutput::ok(format!("{id}: recorded."))
    }
}

fn name(name: &str) -> ToolName {
    ToolName::parse(name).unwrap_or_else(|_| unreachable!("tool names here are constants"))
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}
