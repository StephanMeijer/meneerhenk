//! The planner↔reviewer pairing (#328): the planner drafts a plan and its
//! tracker changes, the reviewer reviews the draft and raises points, the
//! planner answers each and revises, and each resumes its own conversation
//! the next round, until the reviewer raises nothing more or a limit ends
//! it. Then the draft is published, with what the review left open.
//!
//! Both sessions start fresh, keep their conversation for the whole
//! pairing and get only a short update on each swap (#321): their system
//! prompts and tools never change, so every round reuses the cached
//! prefix.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use henk_agent::StopCause;
use henk_domain::plan_review::{PlanPoint, PlanStop};
use henk_domain::run::RunId;
use henk_llm::ChatMessage;
use henk_session::{ResumableSession, RoundOutcome};
use henk_store::{Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::app::App;
use crate::cancel::Cancelled;
use crate::config::PlanningConfig;
use crate::plan_review_tools::PlanHandoff;
use crate::plan_tools::{PlanContext, publish};
use crate::stages;

/// One pairing's sessions and limits.
pub(crate) struct Pairing<'a> {
    pub(crate) app: &'a App,
    pub(crate) run: &'a RunId,
    pub(crate) config: &'a PlanningConfig,
    pub(crate) context: Arc<PlanContext>,
    pub(crate) handoff: Arc<PlanHandoff>,
    pub(crate) planner: ResumableSession,
    pub(crate) reviewer: ResumableSession,
    /// The planner's limit per round, from `[roles.planner]`.
    pub(crate) planner_round: Duration,
    /// The reviewer's limit per round, from `[roles.reviewer]`.
    pub(crate) reviewer_round: Duration,
    /// When the whole pairing must have ended.
    pub(crate) deadline: tokio::time::Instant,
    /// The round under way, or the last one.
    pub(crate) round: u32,
}

/// The issue as the reviewer first reads it: its title and description,
/// as material.
pub(crate) struct IssueMaterial {
    pub(crate) reference: String,
    pub(crate) title: String,
    pub(crate) description: String,
}

impl Pairing<'_> {
    /// Drafts, reviews and revises, then publishes the draft. A planner
    /// that cannot draft a plan fails the run as a planner alone did; a
    /// cancel publishes nothing.
    pub(crate) async fn run(
        mut self,
        opening: ChatMessage,
        issue: &IssueMaterial,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let drafted = self.draft(opening, cancel).await;
        let stop = match drafted {
            Ok(()) => self.rounds(issue, cancel).await,
            Err(error) => {
                self.end_sessions().await;
                return Err(error);
            }
        };
        self.handoff
            .close_all(&format!("the review stopped: {stop}"))
            .await;
        self.end_sessions().await;
        let (store, run) = (&*self.app.store, self.run);
        if let Err(error) = store.end_loop(run, stop.as_str(), self.round).await {
            tracing::warn!(%error, "could not record why the pairing stopped");
        }
        let ledger = self.handoff.ledger();
        let points = ledger.iter().count();
        let line = format!(
            "The plan review ran {} {}, raised {points} {} and stopped: {stop}.",
            self.round,
            if self.round == 1 { "round" } else { "rounds" },
            if points == 1 { "point" } else { "points" },
        );
        info!(run = %run, rounds = self.round, points, %stop, "plan review ended");
        let _ = store.event(run, "info", &line).await;
        if stop == PlanStop::Cancelled {
            return Err(Cancelled.into());
        }
        stages::mark(store, run, Stage::Publish, StageState::Running, "").await;
        publish(&self.context, &ledger.open_note(&stop)).await
    }

    /// The planner's first round: the draft, or why there is none.
    async fn draft(
        &mut self,
        opening: ChatMessage,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let (store, run) = (&*self.app.store, self.run);
        let left = self.left();
        self.planner.set_timeout(left.min(self.planner_round));
        stages::mark(
            store,
            run,
            Stage::Session,
            StageState::Running,
            "drafting: planner",
        )
        .await;
        let outcome = self
            .planner
            .resume(store, run, opening, cancel.clone())
            .await;
        crate::plan::session_result(outcome.stop, self.planner_round.as_secs())?;
        let drafted = self
            .context
            .state
            .lock()
            .is_ok_and(|s| s.draft.plan.is_some());
        if !drafted {
            return Err(anyhow!("the model ended without writing a plan"));
        }
        Ok(())
    }

    /// Takes review rounds until one says to stop, and returns why.
    async fn rounds(&mut self, issue: &IssueMaterial, cancel: &CancellationToken) -> PlanStop {
        let (store, run) = (&*self.app.store, self.run);
        let mut message = format!(
            "Review the plan drafted for issue {}: {}\n\nThe issue's description, as material:\n\n{}\n\nThe draft:\n\n{}",
            issue.reference,
            issue.title,
            issue.description,
            self.describe_draft()
        );
        while self.round < self.config.max_rounds {
            let round = self.round + 1;
            if let Some(stop) = self.out_of_time("reviewer", round) {
                return stop;
            }
            self.round = round;
            self.handoff.start_round(round);
            let doing = format!("round {round}: reviewer");
            stages::mark(store, run, Stage::Session, StageState::Running, doing).await;
            let review = self
                .reviewer
                .resume(store, run, ChatMessage::user(message), cancel.clone())
                .await;
            if let Some(stop) = self.broke_off("reviewer", &review) {
                return stop;
            }
            if !self.handoff.finished() {
                return PlanStop::SessionFailed(format!(
                    "the reviewer did not finish round {round}"
                ));
            }
            let points: Vec<String> = self
                .handoff
                .ledger()
                .raised_in(round)
                .map(PlanPoint::text)
                .collect();
            if points.is_empty() {
                return PlanStop::Converged;
            }

            if let Some(stop) = self.out_of_time("planner", round) {
                return stop;
            }
            let doing = format!("round {round}: planner");
            stages::mark(store, run, Stage::Session, StageState::Running, doing).await;
            let handed = format!(
                "Round {round}. The reviewer's points, as data:\n\n{}\n\nCheck each against the issue and the code, answer every one with answer_point, and change the draft where you take one.",
                points.join("\n\n")
            );
            let revise = self
                .planner
                .resume(store, run, ChatMessage::user(handed), cancel.clone())
                .await;
            self.handoff.close_all("the planner gave no answer").await;
            if let Some(stop) = self.broke_off("planner", &revise) {
                return stop;
            }
            let answers: Vec<String> = self
                .handoff
                .ledger()
                .raised_in(round)
                .map(PlanPoint::answer_line)
                .collect();
            message = format!(
                "Round {}. The planner's answers:\n\n{}\n\nThe draft now:\n\n{}\n\nTo contest a rejection, once, raise the point again with new evidence and name it in reopens. Review again.",
                round + 1,
                answers.join("\n"),
                self.describe_draft()
            );
        }
        PlanStop::MaxRounds(self.config.max_rounds)
    }

    /// The draft as the reviewer reads it.
    fn describe_draft(&self) -> String {
        self.context
            .state
            .lock()
            .map(|s| s.draft.describe())
            .unwrap_or_default()
    }

    /// What is left of the pairing's time.
    fn left(&self) -> Duration {
        self.deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }

    /// The timeout stop when the pairing's time is up; otherwise gives the
    /// next session's round what is left of it, at most a round's limit.
    fn out_of_time(&mut self, who: &str, round: u32) -> Option<PlanStop> {
        let left = self.left();
        if left.is_zero() {
            return Some(PlanStop::Timeout(format!(
                "the time limit of {}s was reached before the {who}'s turn in round {round}",
                self.config.run_timeout_secs
            )));
        }
        let (session, round_limit) = if who == "reviewer" {
            (&mut self.reviewer, self.reviewer_round)
        } else {
            (&mut self.planner, self.planner_round)
        };
        session.set_timeout(left.min(round_limit));
        None
    }

    /// Why the pairing stops when a session's round did not end its turn:
    /// a cancel, a time or turn limit, a model error or refusal.
    fn broke_off(&self, who: &str, round: &RoundOutcome) -> Option<PlanStop> {
        let which = self.round;
        let why = match &round.stop {
            StopCause::EndTurn => return None,
            StopCause::Cancelled => return Some(PlanStop::Cancelled),
            StopCause::Timeout => {
                return Some(PlanStop::Timeout(format!(
                    "the {who} reached its time limit in round {which}"
                )));
            }
            StopCause::MaxTurns => "reached its turn limit".to_owned(),
            StopCause::ModelError(error) => format!("failed: {error}"),
            StopCause::Refused(why) => format!("declined ({why})"),
            StopCause::Stuck { tool, .. } => format!("kept repeating {tool}"),
        };
        Some(PlanStop::SessionFailed(format!(
            "the {who} {why} in round {which}"
        )))
    }

    /// Ends both sessions' records, whichever ran.
    async fn end_sessions(&self) {
        let (store, run) = (&*self.app.store, self.run);
        self.planner.finish(store, run).await;
        self.reviewer.finish(store, run).await;
    }
}

/// The reviewer's `{{workspace}}`: its copy of the repository, or none.
pub(crate) fn reviewer_workspace(workspace: Option<(&str, &str)>) -> String {
    match workspace {
        Some((branch, commit)) => henk_agent::prompts::render(
            henk_agent::prompts::PLAN_REVIEWER_WORKSPACE,
            &[("branch", branch), ("commit", commit)],
        ),
        None => henk_agent::prompts::PLAN_REVIEWER_NO_WORKSPACE.to_owned(),
    }
}
