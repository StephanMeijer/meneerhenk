//! The requirements review of an execute run (#535): before the pull
//! request is opened, a reviewer checks the executor's changes against the
//! issue and the plan in it. The reviewer raises a point for each gap, the
//! executor answers each and revises, and each resumes its own conversation
//! the next round, until the reviewer raises nothing more or a limit ends
//! it. Only a reviewer who has nothing more to raise lets the pull request
//! open; any other ending leaves the run without one, with the points still
//! open said on the issue.
//!
//! It is the planner↔reviewer pairing (#328) with the executor for the
//! planner and the executor's changes for the draft: the same points, the
//! same tools and the same ways to stop. The reviewer reads the changes as
//! they stand in the executor's own workspace, through tools that cannot
//! write, and as a diff Henk makes of them.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{Continuation, ToolSet};
use henk_domain::diff::ReviewDiff;
use henk_domain::plan_review::{PlanPoint, PlanStop, PointAnswer};
use henk_domain::role::Role;
use henk_domain::run::RunId;
use henk_domain::workspace::Change;
use henk_llm::ChatMessage;
use henk_platform::IssueInfo;
use henk_platform::push::{PushWriter, RepoHead};
use henk_session::{ContinuationFactory, ResumableSession, SessionSpec, cache_ttl_for};
use henk_store::{Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::app::App;
use crate::cancel::Cancelled;
use crate::config::{ExecutionConfig, ExecutorConfig};
use crate::diff_tools::{DiffFiles, GetFileDiff, ListChangedFiles};
use crate::execute::no_pull;
use crate::execute_tools::{Proposal, Proposed};
use crate::git::{Checkout, ScratchDir};
use crate::plan::role_limits;
use crate::plan_pairing::broke_off;
use crate::plan_review_tools::{FinishReview, PlanHandoff, RaisePoint, reviewer_continuation};
use crate::roles::equip_and_grant;
use crate::stages;
use crate::workspace::{Workspace, checked_changeset, never_exported};

/// Nudges the executor gets per round before what it left undone ends it:
/// an answer for every point, and the proposal again.
const NUDGES: u32 = 3;

/// What the changes are when the executor changed nothing.
const NOTHING_CHANGED: &str = "the executor changed nothing";

/// What a requirements review is made from.
pub(crate) struct Parts<'a> {
    pub(crate) app: &'a App,
    pub(crate) run: &'a RunId,
    pub(crate) execution: &'a ExecutionConfig,
    pub(crate) executor: &'a ExecutorConfig,
    pub(crate) issue: &'a IssueInfo,
    pub(crate) reference: &'a str,
    pub(crate) repo: &'a str,
    pub(crate) base: &'a RepoHead,
    pub(crate) writer: &'a Arc<dyn PushWriter>,
    /// The executor's workspace, where its changes are.
    pub(crate) workspace: &'a Arc<dyn Workspace>,
    /// Where the executor's proposal waits.
    pub(crate) proposed: &'a Proposed,
    /// The executor's session as an execute run without a review builds it,
    /// plus the tools of its side of the hand-off.
    pub(crate) spec: SessionSpec,
    pub(crate) handoff: Arc<PlanHandoff>,
    /// When the whole run's time is up.
    pub(crate) deadline: tokio::time::Instant,
}

/// Runs the executor and the reviewer to their end. The proposal that
/// comes back is the executor's last, with how the review ended, for the
/// pull request's body.
///
/// # Errors
///
/// An execute run without a pull request: the reviewer was not satisfied
/// (the points still open are said), a session broke off, the time ran out,
/// or the changes could not be read. A cancel is [`Cancelled`].
pub(crate) async fn run(parts: Parts<'_>, cancel: &CancellationToken) -> anyhow::Result<Proposal> {
    let mut review = Review::new(parts).await?;
    let drafted = review.draft(cancel).await;
    let stop = match drafted {
        Ok(()) => review.rounds(cancel).await,
        Err(error) => {
            review.end_sessions().await;
            return Err(error);
        }
    };
    review.conclude(&stop).await;
    match stop {
        PlanStop::Converged => Ok(review.verdict()),
        PlanStop::Cancelled => Err(Cancelled.into()),
        other => Err(no_pull(review.unsatisfied(&other))),
    }
}

/// One review's sessions and limits.
struct Review<'a> {
    app: &'a App,
    run: &'a RunId,
    execution: &'a ExecutionConfig,
    issue: &'a IssueInfo,
    reference: &'a str,
    workspace: &'a Arc<dyn Workspace>,
    proposed: &'a Proposed,
    handoff: Arc<PlanHandoff>,
    max_changed_files: usize,
    /// Henk's own checkout of the default branch, to diff the changes
    /// against.
    checkout: Checkout,
    /// What the reviewer's tools serve.
    diff: Arc<DiffFiles>,
    executor: ResumableSession,
    executor_model: String,
    reviewer: ResumableSession,
    opening: ChatMessage,
    executor_round: Duration,
    reviewer_round: Duration,
    deadline: tokio::time::Instant,
    /// The round under way, or the last one.
    round: u32,
    /// The executor's proposal as it stands.
    proposal: Option<Proposal>,
}

impl<'a> Review<'a> {
    /// The sessions, the reviewer's tools and Henk's checkout.
    async fn new(parts: Parts<'a>) -> anyhow::Result<Self> {
        let Parts {
            app,
            run,
            execution,
            executor,
            issue,
            reference,
            repo,
            base,
            writer,
            workspace,
            proposed,
            mut spec,
            handoff,
            deadline,
        } = parts;
        let reviewer = app
            .settings
            .roles
            .reviewer
            .as_ref()
            .ok_or_else(|| anyhow!("the requirements review needs [roles.reviewer]"))?;
        let reviewer_model = app.model(&reviewer.model)?;
        let credential = writer
            .git_credential()
            .await
            .context("getting a credential for git")?;
        let dir = ScratchDir::new(&format!("henk-execute-{run}-review"))
            .context("making the checkout directory")?;
        let checkout = Checkout::clone_at(
            dir,
            &base.remote,
            &base.default_branch,
            &base.head,
            credential,
        )
        .await
        .context("checking out the default branch to read the changes against")?;
        let diff = Arc::new(DiffFiles::new(Arc::new(ReviewDiff::default())));

        let reviewer_round = reviewer.timeout_secs(Role::Reviewer);
        // The executor has what is left of the run's time in every round, as
        // it has without a review; the role's per-turn limit is the review
        // loop's (#284).
        let executor_round = execution.run_timeout_secs;
        let reviewer_session = reviewer_session(
            app,
            reviewer,
            reviewer_model,
            (reference, repo),
            &Wiring {
                workspace,
                diff: &diff,
                handoff: &handoff,
                partner_secs: executor_round,
            },
        );
        let opening = spec
            .opening
            .drain(..)
            .next()
            .ok_or_else(|| anyhow!("the executor has no opening message"))?;
        spec.limits.cache_ttl = cache_ttl_for(reviewer_round);
        let executor_model = spec.model.model().to_owned();
        let executor_session = ResumableSession::new(
            Role::Executor,
            spec.model,
            spec.system,
            spec.tools,
            spec.limits,
        )
        .with_continuation(executor_continuation(
            Arc::clone(&handoff),
            Arc::clone(proposed),
        ));
        Ok(Self {
            app,
            run,
            execution,
            issue,
            reference,
            workspace,
            proposed,
            handoff,
            max_changed_files: executor.max_changed_files,
            checkout,
            diff,
            executor: executor_session,
            executor_model,
            reviewer: reviewer_session,
            opening,
            executor_round: Duration::from_secs(executor_round),
            reviewer_round: Duration::from_secs(reviewer_round),
            deadline,
            round: 0,
            proposal: None,
        })
    }

    /// The executor's first round: its changes and a proposal, or why
    /// there are none. These end as an execute run without a review ends.
    async fn draft(&mut self, cancel: &CancellationToken) -> anyhow::Result<()> {
        let (store, run) = (&*self.app.store, self.run);
        let left = self.left();
        if left.is_zero() {
            return Err(anyhow!(
                "the time limit of {}s was reached before the executor began",
                self.execution.run_timeout_secs
            ));
        }
        self.executor.set_timeout(left.min(self.executor_round));
        let what = format!("executor, {}", self.executor_model);
        stages::mark(store, run, Stage::Session, StageState::Running, what).await;
        let opening = self.opening.clone();
        let outcome = self
            .executor
            .resume(store, run, opening, cancel.clone())
            .await;
        crate::execute::session_result(outcome.stop, self.execution.run_timeout_secs)?;
        stages::mark(store, run, Stage::Session, StageState::Done, "proposed").await;
        let proposal = self
            .take_proposal()
            .ok_or_else(|| no_pull("the executor ended without proposing a pull request"))?;
        // Nothing to review, or nothing that can be pushed, ends as it does
        // without a review, before the reviewer is paid for.
        if let Err(why) = self.show_changes().await {
            let why = if why == NOTHING_CHANGED && !proposal.left_out.is_empty() {
                format!("{why}: {}", proposal.left_out)
            } else {
                why
            };
            return Err(no_pull(why));
        }
        self.proposal = Some(proposal);
        Ok(())
    }

    /// Takes review rounds until one says to stop, and returns why.
    async fn rounds(&mut self, cancel: &CancellationToken) -> PlanStop {
        let (store, run) = (&*self.app.store, self.run);
        while self.round < self.execution.requirements_max_rounds {
            let round = self.round + 1;
            if let Some(stop) = self.out_of_time("reviewer", round) {
                return stop;
            }
            let changed = match self.show_changes().await {
                Ok(list) => list,
                Err(why) => return PlanStop::SessionFailed(why),
            };
            self.round = round;
            self.handoff.start_round(round);
            let message = self.review_message(round, &changed);
            stages::mark(
                store,
                run,
                Stage::Session,
                StageState::Running,
                format!("round {round}: reviewer"),
            )
            .await;
            let review = self
                .reviewer
                .resume(store, run, ChatMessage::user(message), cancel.clone())
                .await;
            if let Some(stop) = broke_off("reviewer", round, &review) {
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
            if let Some(stop) = self.revise(round, &points, cancel).await {
                return stop;
            }
        }
        PlanStop::MaxRounds(self.execution.requirements_max_rounds)
    }

    /// The executor's turn in round `round`: it answers `points`, changes
    /// the code where it takes one and proposes again. Why the review stops
    /// if it cannot go on.
    async fn revise(
        &mut self,
        round: u32,
        points: &[String],
        cancel: &CancellationToken,
    ) -> Option<PlanStop> {
        let (store, run) = (&*self.app.store, self.run);
        if let Some(stop) = self.out_of_time("executor", round) {
            return Some(stop);
        }
        // A proposal from before the points is not one for after them.
        let _ = self.take_proposal();
        stages::mark(
            store,
            run,
            Stage::Session,
            StageState::Running,
            format!("round {round}: executor"),
        )
        .await;
        let handed = format!(
            "Round {round}. The reviewer's points, as data:\n\n{}\n\nCheck each against the issue and the code, answer every one with answer_point, change the code where you take one, and propose the pull request again with propose_pull_request.",
            points.join("\n\n")
        );
        let revise = self
            .executor
            .resume(store, run, ChatMessage::user(handed), cancel.clone())
            .await;
        self.handoff.close_all("the executor gave no answer").await;
        if let Some(stop) = broke_off("executor", round, &revise) {
            return Some(stop);
        }
        match self.take_proposal() {
            Some(proposal) => {
                self.proposal = Some(proposal);
                None
            }
            None => Some(PlanStop::SessionFailed(format!(
                "the executor did not propose the pull request again in round {round}"
            ))),
        }
    }

    /// What the reviewer is told at the start of round `round`.
    fn review_message(&self, round: u32, files: &str) -> String {
        let proposal = self.proposal.as_ref();
        let summary = proposal.map_or("", |p| p.summary.as_str());
        let left_out = proposal.map_or("", |p| p.left_out.as_str());
        let left_out = if left_out.trim().is_empty() {
            "nothing"
        } else {
            left_out
        };
        let changes = format!(
            "The executor's summary, as data: {summary}\n\nWhat it says it left out, as data: {left_out}\n\nThe files it changed:\n{files}"
        );
        if round == 1 {
            return format!(
                "Check the changes the executor made for issue {}: {}\n\nThe issue's description, with any plan in it, as material:\n\n{}\n\n{changes}\n\nRead each diff with get_file_diff and the code around it, check the changes against what the issue and its plan ask, and raise a point for each gap with raise_point. End with finish_round.",
                self.reference, self.issue.title, self.issue.body
            );
        }
        let answers: Vec<String> = self
            .handoff
            .ledger()
            .raised_in(round - 1)
            .map(PlanPoint::answer_line)
            .collect();
        format!(
            "Round {round}. The executor's answers:\n\n{}\n\n{changes}\n\nTo contest a rejection, once, raise the point again with new evidence and name it in reopens. Check again.",
            answers.join("\n")
        )
    }

    /// The executor's changes as a diff, served to the reviewer's tools from
    /// now on; the list of changed files. The error is why the changes
    /// cannot be reviewed.
    async fn show_changes(&self) -> Result<String, String> {
        let changes = self.changes().await?;
        let text = self
            .checkout
            .diff_of(&changes)
            .await
            .map_err(|error| format!("making the diff of the changes: {error}"))?;
        let diff = Arc::new(ReviewDiff::from_unified(&text));
        let list = diff.render_list();
        self.diff.set(diff);
        Ok(list)
    }

    /// The changeset as the push rules take it.
    async fn changes(&self) -> Result<Vec<Change>, String> {
        let exported = self
            .workspace
            .export()
            .await
            .map_err(|error| format!("listing the changes: {error}"))?;
        if exported.is_empty() {
            return Err(NOTHING_CHANGED.to_owned());
        }
        checked_changeset(exported, self.max_changed_files)
            .map_err(|why| format!("the executor's changes cannot be pushed: {why}"))
    }

    /// The executor's proposal, if it made one since the last was taken.
    fn take_proposal(&self) -> Option<Proposal> {
        self.proposed.lock().ok().and_then(|mut p| p.take())
    }

    /// What is left of the review's time.
    fn left(&self) -> Duration {
        self.deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }

    /// The timeout stop when the time is up; otherwise gives the next
    /// session's round what is left of it, at most a round's limit.
    fn out_of_time(&mut self, who: &str, round: u32) -> Option<PlanStop> {
        let left = self.left();
        if left.is_zero() {
            return Some(PlanStop::Timeout(format!(
                "the time limit of {}s was reached before the {who}'s turn in round {round}",
                self.execution.run_timeout_secs
            )));
        }
        let (session, limit) = if who == "reviewer" {
            (&mut self.reviewer, self.reviewer_round)
        } else {
            (&mut self.executor, self.executor_round)
        };
        session.set_timeout(left.min(limit));
        None
    }

    /// Records how the review ended: the points still unanswered are
    /// closed, the sessions end, and the run keeps why and how many rounds.
    async fn conclude(&self, stop: &PlanStop) {
        self.handoff
            .close_all(&format!("the review stopped: {stop}"))
            .await;
        self.end_sessions().await;
        let (store, run) = (&*self.app.store, self.run);
        if let Err(error) = store.end_loop(run, stop.as_str(), self.round).await {
            tracing::warn!(%error, "could not record why the review stopped");
        }
        let ledger = self.handoff.ledger();
        let points = ledger.iter().count();
        let line = format!(
            "The requirements review ran {} {}, raised {points} {} and stopped: {stop}.",
            self.round,
            if self.round == 1 { "round" } else { "rounds" },
            if points == 1 { "point" } else { "points" },
        );
        info!(run = %run, rounds = self.round, points, %stop, "requirements review ended");
        let _ = store.event(run, "info", &line).await;
    }

    /// Ends both sessions' records, whichever ran.
    async fn end_sessions(&self) {
        let (store, run) = (&*self.app.store, self.run);
        self.executor.finish(store, run).await;
        self.reviewer.finish(store, run).await;
    }

    /// The executor's last proposal with how the review went, for the pull
    /// request's body.
    fn verdict(&mut self) -> Proposal {
        let mut proposal = self.proposal.take().unwrap_or_else(|| Proposal {
            title: None,
            summary: String::new(),
            left_out: String::new(),
            commits: None,
            requirements: String::new(),
        });
        proposal.requirements = summary_of(&self.handoff, self.round);
        proposal
    }

    /// Why there is no pull request, for the issue: the review's ending and
    /// the points still open.
    fn unsatisfied(&self, stop: &PlanStop) -> String {
        let ledger = self.handoff.ledger();
        let mut text = format!("the requirements review did not accept the changes: {stop}");
        let open = ledger.still_open();
        if open.is_empty() {
            return text;
        }
        text.push_str(". Points still open:");
        for point in open {
            let _ = write!(
                text,
                "\n- {} ({}): {}",
                point.raised.about, point.raised.claim, point.raised.why
            );
            if let Some(answer) = &point.answer {
                let _ = write!(text, " Executor: {} ({}).", answer.as_str(), answer.text());
            }
        }
        text
    }
}

/// What the reviewer's tools are wired to.
struct Wiring<'a> {
    /// The executor's workspace, read-only for the reviewer.
    workspace: &'a Arc<dyn Workspace>,
    /// The changes as a diff.
    diff: &'a Arc<DiffFiles>,
    handoff: &'a Arc<PlanHandoff>,
    /// How long the executor's round may take, for the prompt cache.
    partner_secs: u64,
}

/// The reviewer's session: the changes, the code around them to read, and
/// its side of the hand-off. It cannot write anything.
fn reviewer_session(
    app: &App,
    reviewer: &crate::config::RoleConfig,
    model: Arc<dyn henk_llm::ModelClient>,
    (reference, repo): (&str, &str),
    wiring: &Wiring<'_>,
) -> ResumableSession {
    let mut system = format!(
        "{}\n\n{}",
        henk_agent::prompts::PERSONA,
        henk_agent::prompts::render(
            henk_agent::prompts::REQUIREMENTS_REVIEWER,
            &[("ref", reference), ("repo", repo)],
        )
    );
    let reading = never_exported(Arc::clone(wiring.workspace));
    let mut tools = ToolSet::new();
    tools.add(ListChangedFiles(Arc::clone(wiring.diff)));
    tools.add(GetFileDiff(Arc::clone(wiring.diff)));
    crate::code_tools::add(&mut tools, &reading);
    tools.add(RaisePoint(Arc::clone(wiring.handoff)));
    tools.add(FinishReview(Arc::clone(wiring.handoff)));
    let tools = equip_and_grant(
        Role::Reviewer,
        &mut system,
        tools,
        app.settings.skills.select(&reviewer.skills),
        &reviewer.tools(Role::Reviewer),
    );
    ResumableSession::new(
        Role::Reviewer,
        model,
        system,
        tools,
        role_limits(app, reviewer, Role::Reviewer, wiring.partner_secs),
    )
    .with_continuation(reviewer_continuation(Arc::clone(wiring.handoff)))
}

/// How a review that accepted the changes went, in a few lines for the pull
/// request: how many rounds, and what became of the points.
fn summary_of(handoff: &PlanHandoff, rounds: u32) -> String {
    let ledger = handoff.ledger();
    let points: Vec<&PlanPoint> = ledger.iter().collect();
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            one.to_owned()
        } else {
            many.to_owned()
        }
    };
    let rounds_text = format!("{rounds} {}", plural(rounds as usize, "round", "rounds"));
    if points.is_empty() {
        return format!(
            "The reviewer checked the changes against the issue and its plan in {rounds_text} and raised no point."
        );
    }
    let taken = points
        .iter()
        .filter(|p| matches!(p.answer, Some(PointAnswer::Taken { .. })))
        .count();
    let rejected: Vec<&&PlanPoint> = points
        .iter()
        .filter(|p| matches!(p.answer, Some(PointAnswer::Rejected { .. })))
        .collect();
    let mut text = format!(
        "The reviewer checked the changes against the issue and its plan in {rounds_text} and raised {} {}: {taken} taken, {} rejected.",
        points.len(),
        plural(points.len(), "point", "points"),
        rejected.len()
    );
    for point in rejected {
        let reason = point.answer.as_ref().map_or("", PointAnswer::text);
        let _ = write!(
            text,
            "\n\n- Rejected, {} ({}): {reason}",
            point.raised.about, point.raised.claim
        );
    }
    text
}

/// The executor's nudge: an answer for every point of the round, and the
/// pull request proposed again, at most [`NUDGES`] times.
fn executor_continuation(handoff: Arc<PlanHandoff>, proposed: Proposed) -> ContinuationFactory {
    Arc::new(move || {
        let handoff = Arc::clone(&handoff);
        let proposed = Arc::clone(&proposed);
        let nudged = AtomicU32::new(0);
        let continuation: Continuation = Box::new(move |_| {
            let open: Vec<String> = handoff
                .state
                .lock()
                .map(|s| s.ledger.open().iter().map(|p| p.id.to_string()).collect())
                .unwrap_or_default();
            let proposing = proposed.lock().is_ok_and(|p| p.is_some());
            if (open.is_empty() && proposing) || nudged.fetch_add(1, Ordering::SeqCst) >= NUDGES {
                return None;
            }
            Some(if open.is_empty() {
                "End with propose_pull_request: a summary of what you changed, and what you left out and why. Only then end your turn.".to_owned()
            } else {
                format!(
                    "These points have no answer yet: {}. Answer each one with answer_point, then propose the pull request again with propose_pull_request. Only then end your turn.",
                    open.join(", ")
                )
            })
        });
        continuation
    })
}
