//! The autonomous ticket run (#536): one issue driven through a plan, an
//! execute run and a review of the pull request it opens, with nobody
//! starting each step.
//!
//! The ticket run is a run of its own that owns the three. Each of them is
//! the run it always was, with its own id, stages and sessions; the ticket
//! run starts them in order, links them (`run_children`), and records where
//! it stands in stages of its own (`plan`, `execute`, `review`).
//!
//! * The plan goes on only when its review converged: the reviewer raised
//!   nothing more. A plan published with its points still open ends the
//!   ticket run.
//! * The execute run takes the plan from the issue, as it always does, and
//!   the pull request it opens is the review's.
//! * The review is the reviewer and executor loop, started through the
//!   coordinator, so it waits for a slot like any other and a newer commit
//!   still replaces it. A pull request that a person took over, or that
//!   another run reviews, hands the ticket run over: it ends well.
//!
//! The ticket run leaves exactly one comment on the issue when a step fails
//! or hits a limit, saying which and why: the plan and execute runs are
//! started quiet (`Comments::Caller`) and the review posts nothing on the
//! issue. The whole run has one time limit, `[autonomous] run_timeout_secs`,
//! on top of each step's own. A stage told to stop, by a person's cancel or
//! the limit, is always waited for, so it writes its own ending.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use henk_domain::marker::ModelId;
use henk_domain::plan_review::PlanStop;
use henk_domain::queue::Decision;
use henk_domain::review::{CheckConclusion, CommitSha, Stopped};
use henk_domain::run::{RunId, RunKind};
use henk_platform::{IssueTarget, IssueWriter};
use henk_store::{NewRun, RunStatus, Stage, StageState};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

use crate::app::App;
use crate::cancel::cancelled_reason;
use crate::config::Settings;
use crate::coordinator::Coordinator;
use crate::execute::{ExecuteReport, ExecuteRequest, OpenedPullRequest, run_execute};
use crate::ids::new_run_id;
use crate::issue_runs::{Comments, IssueThread, OnIssue, RunFailed, open_issue};
use crate::liveness::KeepAlive;
use crate::plan::{PlanReport, PlanRequest, run_plan};
use crate::review::{
    CancelledBy, Interrupted, NotReviewable, ReviewReport, ReviewRequest, Superseded,
};
use crate::stages;

/// How long a ticket run waits for a stage it told to stop to write its
/// ending, before it goes on without it.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// Who a stage is cancelled by when the whole run's time is up: not a
/// person, so the stage ends as cancelled, not failed, and the ticket run
/// says why.
const LIMIT_BY: &str = "ticket:time-limit";

/// A request for one ticket run.
#[derive(Debug, Clone)]
pub struct TicketRequest {
    /// The issue.
    pub target: IssueTarget,
    /// A note from the requester, given to the planner and the executor.
    pub note: Option<String>,
    /// What started it.
    pub trigger: String,
    /// The run id to use, when the caller already announced one.
    pub run: Option<RunId>,
    /// Who asked, for the run records: `github:<id>` from the dashboard,
    /// `mcp:<token>` over MCP.
    pub requester: Option<String>,
}

/// How long a ticket run has.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// For the plan, the execute run and the review together.
    pub run: Duration,
    /// For a stage told to stop to write its ending.
    pub grace: Duration,
}

impl Limits {
    /// The limits `[autonomous]` sets.
    ///
    /// # Errors
    ///
    /// Returns an error when autonomous runs are not configured.
    pub fn from_settings(settings: &Settings) -> anyhow::Result<Self> {
        let autonomous = settings
            .autonomous
            .as_ref()
            .ok_or_else(|| anyhow!("autonomous runs are not configured"))?;
        Ok(Self {
            run: Duration::from_secs(autonomous.run_timeout_secs),
            grace: STOP_GRACE,
        })
    }
}

/// The three steps of a ticket run, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The plan, and the review of the plan.
    Plan,
    /// The executor takes the issue to a pull request.
    Execute,
    /// The reviewer and the executor review that pull request.
    Review,
}

impl Step {
    const ALL: [Self; 3] = [Self::Plan, Self::Execute, Self::Review];

    /// The ticket run's stage for it.
    fn stage(self) -> Stage {
        match self {
            Self::Plan => Stage::Plan,
            Self::Execute => Stage::Execute,
            Self::Review => Stage::Review,
        }
    }

    /// What the run was doing in it, for the comment.
    fn doing(self) -> &'static str {
        match self {
            Self::Plan => "planning",
            Self::Execute => "execution",
            Self::Review => "the review",
        }
    }
}

/// How a ticket run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The pull request was reviewed and the review passed.
    Passed,
    /// A person's work, or another run's, took over the pull request, so
    /// the ticket run's job is done; why.
    HandedOver(String),
    /// A step failed or hit a limit; why.
    Stopped {
        /// Where.
        step: Step,
        /// Why, in Henk's words.
        why: String,
    },
    /// A person cancelled it, by this id.
    Cancelled(String),
    /// Henk stopped.
    Interrupted,
}

impl Ending {
    /// Whether the ticket run did what it was asked.
    #[must_use]
    pub fn is_well(&self) -> bool {
        matches!(self, Self::Passed | Self::HandedOver(_))
    }
}

impl fmt::Display for Ending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passed => f.write_str("the pull request was reviewed and the review passed"),
            Self::HandedOver(why) => write!(f, "handed over: {why}"),
            Self::Stopped { step, why } => {
                write!(f, "stopped during {}: {why}", step.doing())
            }
            Self::Cancelled(by) => f.write_str(&cancelled_reason(by)),
            Self::Interrupted => f.write_str("interrupted"),
        }
    }
}

/// What a ticket run reports.
#[derive(Debug)]
pub struct TicketReport {
    /// The ticket run.
    pub run: RunId,
    /// How it ended.
    pub ending: Ending,
    /// The pull request the execute run opened, when it got that far.
    pub pull: Option<OpenedPullRequest>,
}

/// The runs a ticket run starts. The seam of the orchestrator: the real
/// ones are [`Live`]; a test scripts them.
#[async_trait::async_trait]
pub(crate) trait Stages: Send + Sync {
    /// Plans the issue. The run is `request.run`.
    async fn plan(&self, request: PlanRequest) -> anyhow::Result<PlanReport>;

    /// Takes the issue to a pull request. The run is `request.run`.
    async fn execute(&self, request: ExecuteRequest) -> anyhow::Result<ExecuteReport>;

    /// Submits the review of the pull request. The receiver answers when
    /// the decision is `Start` or `Supersede`; see
    /// [`Coordinator::submit_review_awaited`].
    fn review(
        &self,
        request: ReviewRequest,
        commit: CommitSha,
    ) -> (
        Decision,
        RunId,
        oneshot::Receiver<anyhow::Result<ReviewReport>>,
    );

    /// Cancels a stage's run on behalf of `by`, as a person's cancel does.
    /// False when the run is not one running here.
    fn stop(&self, run: &RunId, by: String) -> bool;
}

/// The stages as they run for real.
pub(crate) struct Live {
    coordinator: Arc<Coordinator>,
}

impl Live {
    /// The real stages, through `coordinator`.
    pub(crate) fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait::async_trait]
impl Stages for Live {
    async fn plan(&self, request: PlanRequest) -> anyhow::Result<PlanReport> {
        let app = self.coordinator.app();
        let run = request
            .run
            .clone()
            .ok_or_else(|| anyhow!("a stage's run needs its id"))?;
        // Cancellable by its id, so a stop reaches it.
        let cancel = app.shutdown.child_token();
        let _registered = app.cancels.register(run, cancel.clone());
        run_plan(app, request, cancel).await
    }

    async fn execute(&self, request: ExecuteRequest) -> anyhow::Result<ExecuteReport> {
        let app = self.coordinator.app();
        let run = request
            .run
            .clone()
            .ok_or_else(|| anyhow!("a stage's run needs its id"))?;
        let cancel = app.shutdown.child_token();
        let _registered = app.cancels.register(run, cancel.clone());
        run_execute(app, request, cancel).await
    }

    fn review(
        &self,
        request: ReviewRequest,
        commit: CommitSha,
    ) -> (
        Decision,
        RunId,
        oneshot::Receiver<anyhow::Result<ReviewReport>>,
    ) {
        self.coordinator.submit_review_awaited(request, commit)
    }

    fn stop(&self, run: &RunId, by: String) -> bool {
        self.coordinator.cancel(run, by)
    }
}

/// Why a ticket run stops a stage it started.
enum Cause {
    /// A person cancelled the ticket run, by this id.
    Cancelled(String),
    /// Henk is stopping.
    Shutdown,
    /// The whole run's time is up.
    Limit,
}

/// What a stage did while the ticket run waited for it.
enum Stepped<T> {
    /// It ended by itself.
    Returned(anyhow::Result<T>),
    /// The ticket run stopped it, and ends as this.
    Stopped(Ending),
}

/// What the run has made so far, for the ending.
#[derive(Default)]
struct Progress {
    /// A plan is on the issue.
    plan_published: bool,
    /// The execute run opened this.
    pull: Option<OpenedPullRequest>,
}

/// Drives `request`'s issue through a plan, an execute run and a review.
/// Every ticket run ends (#536): with the pull request reviewed and passed,
/// handed over, or one comment on the issue saying which step stopped it
/// and why. `Err` only when the run's own record cannot be written.
///
/// # Errors
///
/// Returns an error when autonomous runs, the allowlist or the issue do not
/// allow it (before any run exists), or when the run's ending cannot be
/// recorded.
#[instrument(skip_all, fields(repo = %request.target.repo.path(), issue = request.target.number))]
pub(crate) async fn run_ticket(
    app: &App,
    stages: &dyn Stages,
    request: TicketRequest,
    cancel: CancellationToken,
    limits: Limits,
) -> anyhow::Result<TicketReport> {
    if app.settings.autonomous.is_none() {
        return Err(anyhow!("autonomous runs are not configured"));
    }
    let platform = request.target.repo.platform();
    let writer = app.issue_writer(&request.target.repo)?;
    if !app.settings.allowlist.allows(&request.target.repo) {
        return Err(anyhow!("{} is not on the allowlist", request.target.repo));
    }
    open_issue(writer.as_ref(), &request.target, "work autonomously on").await?;

    let run = request.run.clone().unwrap_or_else(new_run_id);
    let link = app.settings.run_link(&run);
    app.store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Ticket,
            platform,
            repo: request.target.repo.path(),
            target: request.target.number,
            commit: None,
            requester: request.requester.clone(),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await?;
    info!(run = %run, "autonomous run started");
    let _alive = KeepAlive::start(Arc::clone(&app.store), &app.live_runs, run.clone());
    let store = &*app.store;
    stages::requested(
        store,
        &run,
        None,
        &request.trigger,
        request.requester.as_deref(),
    )
    .await;
    let what = format!(
        "#{} through plan, execution and review",
        request.target.number
    );
    stages::mark(store, &run, Stage::Started, StageState::Done, what).await;

    let ticket = Ticket {
        app,
        stages,
        request: &request,
        run: run.clone(),
        link,
        writer,
        model: ModelId::parse("orchestrator").unwrap_or_else(|_| unreachable!("constant")),
        cancel,
        deadline: Instant::now() + limits.run,
        limits,
    };
    let mut progress = Progress::default();
    let ending = if let Err(error) = ticket.ready() {
        // Something the stages need is missing: nothing starts, and the run
        // says so. That is for whoever runs Henk, not for the issue.
        let why = format!("{error:#}");
        ticket.fail_quietly(&why).await?;
        Ending::Stopped {
            step: Step::Plan,
            why,
        }
    } else {
        let ending = ticket.walk(&mut progress).await;
        ticket.conclude(&ending, &progress).await?;
        ending
    };
    info!(run = %run, %ending, "autonomous run ended");
    Ok(TicketReport {
        run,
        ending,
        pull: progress.pull,
    })
}

/// One ticket run, as every step of it sees it.
struct Ticket<'a> {
    app: &'a App,
    stages: &'a dyn Stages,
    request: &'a TicketRequest,
    run: RunId,
    link: String,
    writer: Arc<dyn IssueWriter>,
    model: ModelId,
    /// Fires when a person cancels the ticket run, or Henk stops.
    cancel: CancellationToken,
    deadline: Instant,
    limits: Limits,
}

impl Ticket<'_> {
    /// That the stages will find what they need: the three agents' models,
    /// and the ways to the platform. Checked first so a missing one stops
    /// the run before it plans, not after.
    fn ready(&self) -> anyhow::Result<()> {
        let roles = &self.app.settings.roles;
        let repo = &self.request.target.repo;
        for (role, model) in [
            ("planner", roles.planner.as_ref().map(|r| r.model.as_str())),
            (
                "reviewer",
                roles.reviewer.as_ref().map(|r| r.model.as_str()),
            ),
            (
                "executor",
                roles.executor.as_ref().map(|r| r.model.as_str()),
            ),
        ] {
            let model = model.ok_or_else(|| anyhow!("no [roles.{role}]"))?;
            self.app.model(model)?;
        }
        self.app.writer(repo)?;
        self.app.push_writer(repo)?;
        Ok(())
    }

    fn on_issue(&self) -> OnIssue<'_> {
        OnIssue {
            app: self.app,
            thread: IssueThread {
                writer: &self.writer,
                target: &self.request.target,
                comments: Comments::Own,
            },
            run: &self.run,
            link: &self.link,
            model: &self.model,
            what: "the autonomous run",
        }
    }

    /// Goes through the three steps; how it ends.
    async fn walk(&self, progress: &mut Progress) -> Ending {
        // The plan.
        if let Some(ending) = self.halted(Step::Plan) {
            return ending;
        }
        let plan_run = new_run_id();
        self.begin(Step::Plan, &plan_run).await;
        let request = PlanRequest {
            target: self.request.target.clone(),
            note: self.request.note.clone(),
            trigger: self.trigger(),
            run: Some(plan_run.clone()),
            requester: self.request.requester.clone(),
            comments: Comments::Caller,
        };
        let report = match self
            .step(Step::Plan, &plan_run, self.stages.plan(request))
            .await
        {
            Stepped::Stopped(ending) => return self.ended(Step::Plan, ending).await,
            Stepped::Returned(Err(error)) => {
                return self.ended(Step::Plan, from_error(Step::Plan, &error)).await;
            }
            Stepped::Returned(Ok(report)) => report,
        };
        progress.plan_published = report.planned;
        if report.stop != PlanStop::Converged {
            let why = format!("the plan review did not settle: {}", report.stop);
            return self
                .ended(
                    Step::Plan,
                    Ending::Stopped {
                        step: Step::Plan,
                        why,
                    },
                )
                .await;
        }
        let detail = format!("run {plan_run}: the plan review converged");
        self.settled(Step::Plan, detail).await;

        // The execute run, from the plan on the issue.
        if let Some(ending) = self.halted(Step::Execute) {
            return ending;
        }
        let execute_run = new_run_id();
        self.begin(Step::Execute, &execute_run).await;
        let request = ExecuteRequest {
            target: self.request.target.clone(),
            note: self.request.note.clone(),
            trigger: self.trigger(),
            run: Some(execute_run.clone()),
            requester: self.request.requester.clone(),
            comments: Comments::Caller,
        };
        let opened = match self
            .step(Step::Execute, &execute_run, self.stages.execute(request))
            .await
        {
            Stepped::Stopped(ending) => return self.ended(Step::Execute, ending).await,
            Stepped::Returned(Err(error)) => {
                return self
                    .ended(Step::Execute, from_error(Step::Execute, &error))
                    .await;
            }
            Stepped::Returned(Ok(report)) => report,
        };
        let pull = opened.pull.clone();
        progress.pull = Some(pull.clone());
        let detail = format!(
            "run {execute_run}: pull request #{} opened: {}",
            pull.target.number, pull.url
        );
        self.settled(Step::Execute, detail).await;

        // The review of that pull request.
        if let Some(ending) = self.halted(Step::Review) {
            return ending;
        }
        let review = ReviewRequest {
            trigger: self.trigger(),
            ..opened.review(self.request.requester.clone())
        };
        let (decision, review_run, ended) = self.stages.review(review, pull.commit.clone());
        self.begin(Step::Review, &review_run).await;
        if matches!(decision, Decision::Join | Decision::After) {
            // Another review of this pull request is under way, or waits for
            // a rebase of it: that run's, not this one's, to report.
            let why = format!("review run {review_run} already covers the pull request");
            return self.ended(Step::Review, Ending::HandedOver(why)).await;
        }
        let work = async move {
            ended
                .await
                .unwrap_or_else(|_| Err(anyhow!("the review ended without a report")))
        };
        match self.step(Step::Review, &review_run, work).await {
            Stepped::Stopped(ending) => self.ended(Step::Review, ending).await,
            Stepped::Returned(result) => self.ended(Step::Review, review_ending(result)).await,
        }
    }

    /// Waits for `work`, the stage run `child` is doing, unless the ticket
    /// run has to stop it first: a person cancels the ticket run, Henk
    /// stops, or the whole run's time is up. A stage that is told to stop is
    /// waited for, so it writes its own ending, but not for long.
    async fn step<T>(
        &self,
        step: Step,
        child: &RunId,
        work: impl Future<Output = anyhow::Result<T>>,
    ) -> Stepped<T> {
        let mut work = std::pin::pin!(work);
        let cause = tokio::select! {
            biased;
            result = &mut work => return Stepped::Returned(result),
            () = self.cancel.cancelled() => match self.app.cancels.cancelled_by(&self.run) {
                Some(by) => {
                    self.stages.stop(child, by.clone());
                    Cause::Cancelled(by)
                }
                // A shutdown has reached the stage's own token already.
                None => Cause::Shutdown,
            },
            () = tokio::time::sleep_until(self.deadline) => {
                self.stages.stop(child, LIMIT_BY.to_owned());
                Cause::Limit
            }
        };
        if tokio::time::timeout(self.limits.grace, &mut work)
            .await
            .is_err()
        {
            warn!(run = %self.run, child = %child, "a stage did not end after it was told to stop");
        }
        Stepped::Stopped(self.ending_for(step, cause))
    }

    /// The ending a stop for `cause` in `step` is.
    fn ending_for(&self, step: Step, cause: Cause) -> Ending {
        match cause {
            Cause::Cancelled(by) => Ending::Cancelled(by),
            Cause::Shutdown => Ending::Interrupted,
            Cause::Limit => Ending::Stopped {
                step,
                why: format!(
                    "the time limit of {}s for the whole run was reached",
                    self.limits.run.as_secs()
                ),
            },
        }
    }

    /// Whether the run has to stop before `step` starts.
    fn halted(&self, step: Step) -> Option<Ending> {
        if self.cancel.is_cancelled() {
            return Some(
                self.ending_for(
                    step,
                    self.app
                        .cancels
                        .cancelled_by(&self.run)
                        .map_or(Cause::Shutdown, Cause::Cancelled),
                ),
            );
        }
        (Instant::now() >= self.deadline).then(|| self.ending_for(step, Cause::Limit))
    }

    /// What the stage runs record as their trigger.
    fn trigger(&self) -> String {
        format!("autonomous run {}", self.run)
    }

    /// `child` is the run for `step`: linked, and the stage is going.
    async fn begin(&self, step: Step, child: &RunId) {
        if let Err(error) = self
            .app
            .store
            .link_child(&self.run, step.stage(), child)
            .await
        {
            warn!(%error, run = %self.run, child = %child, "could not link the run to its step");
        }
        stages::mark(
            &*self.app.store,
            &self.run,
            step.stage(),
            StageState::Running,
            format!("run {child}"),
        )
        .await;
    }

    /// `step` is done.
    async fn settled(&self, step: Step, detail: String) {
        stages::mark(
            &*self.app.store,
            &self.run,
            step.stage(),
            StageState::Done,
            detail,
        )
        .await;
    }

    /// `step` ended as `ending`: its stage says so. Returns `ending`.
    async fn ended(&self, step: Step, ending: Ending) -> Ending {
        let (state, detail) = match &ending {
            Ending::Passed => (StageState::Done, "the review passed".to_owned()),
            Ending::HandedOver(why) => (StageState::Done, why.clone()),
            Ending::Stopped { why, .. } => (StageState::Failed, why.clone()),
            Ending::Cancelled(by) => (StageState::Skipped, cancelled_reason(by)),
            Ending::Interrupted => (StageState::Failed, "interrupted".to_owned()),
        };
        stages::mark(&*self.app.store, &self.run, step.stage(), state, detail).await;
        ending
    }

    /// Records the ending, and says it on the issue when it is a failure.
    async fn conclude(&self, ending: &Ending, progress: &Progress) -> anyhow::Result<()> {
        let store = &*self.app.store;
        match ending {
            Ending::Passed | Ending::HandedOver(_) => {
                let summary = match (ending, &progress.pull) {
                    (Ending::Passed, Some(pull)) => {
                        format!(
                            "pull request #{} reviewed and passed: {}",
                            pull.target.number, pull.url
                        )
                    }
                    (Ending::HandedOver(why), _) => format!("handed over: {why}"),
                    _ => ending.to_string(),
                };
                stages::finished(store, &self.run, &summary).await;
                store
                    .finish_run(&self.run, RunStatus::Finished, Some(&summary), None)
                    .await?;
                Ok(())
            }
            Ending::Stopped { step, why } => {
                self.skip_unreached("the run stopped first").await;
                let comment = stopped_comment(*step, why, progress);
                clean(self.on_issue().failed(&comment, why).await)
            }
            Ending::Cancelled(by) => {
                self.skip_unreached("the run was cancelled first").await;
                clean(self.on_issue().cancelled(by).await)
            }
            Ending::Interrupted => {
                self.skip_unreached("Henk stopped first").await;
                clean(self.on_issue().interrupted().await)
            }
        }
    }

    /// The steps never begun end as skipped, for the pipeline.
    async fn skip_unreached(&self, why: &str) {
        let store = &*self.app.store;
        let begun = store.stages(&self.run).await.unwrap_or_default();
        for step in Step::ALL {
            if !begun.iter().any(|s| s.stage == step.stage()) {
                stages::mark(
                    store,
                    &self.run,
                    step.stage(),
                    StageState::Skipped,
                    format!("not reached: {why}"),
                )
                .await;
            }
        }
    }

    /// The run cannot start; it ends failed with `why` and nothing is said
    /// on the issue.
    async fn fail_quietly(&self, why: &str) -> anyhow::Result<()> {
        let store = &*self.app.store;
        self.skip_unreached("the run did not start").await;
        stages::failed(store, &self.run).await;
        store
            .finish_run(&self.run, RunStatus::Failed, None, Some(why))
            .await?;
        Ok(())
    }
}

/// What an ending's `failed`, `cancelled` or `interrupted` returns is the
/// run's own ending, which is fine; anything else is the store failing.
fn clean(error: anyhow::Error) -> anyhow::Result<()> {
    if error.is::<RunFailed>() || error.is::<CancelledBy>() || error.is::<Interrupted>() {
        Ok(())
    } else {
        Err(error)
    }
}

/// How a plan or an execute run's error ends the ticket run.
fn from_error(step: Step, error: &anyhow::Error) -> Ending {
    if let Some(CancelledBy(by)) = error.downcast_ref::<CancelledBy>() {
        return Ending::Cancelled(by.clone());
    }
    if error.is::<Interrupted>() {
        return Ending::Interrupted;
    }
    let why = error
        .downcast_ref::<RunFailed>()
        .map_or_else(|| format!("{error:#}"), |failed| failed.reason.clone());
    Ending::Stopped { step, why }
}

/// How a review's report, or error, ends the ticket run.
fn review_ending(result: anyhow::Result<ReviewReport>) -> Ending {
    let stopped = |why: String| Ending::Stopped {
        step: Step::Review,
        why,
    };
    match result {
        Ok(report) => match report.outcome {
            Some(outcome) if outcome.stopped == Some(Stopped::Superseded) => Ending::HandedOver(
                "a newer commit replaced the review of the pull request".to_owned(),
            ),
            Some(outcome) if outcome.check_conclusion() == CheckConclusion::Success => {
                Ending::Passed
            }
            Some(outcome) => stopped(outcome.headline()),
            None => stopped(
                report
                    .summary
                    .unwrap_or_else(|| "the review ended without an outcome".to_owned()),
            ),
        },
        Err(error) => {
            if let Some(CancelledBy(by)) = error.downcast_ref::<CancelledBy>() {
                Ending::Cancelled(by.clone())
            } else if error.is::<Interrupted>() {
                Ending::Interrupted
            } else if error.is::<Superseded>() {
                Ending::HandedOver(
                    "a newer commit or a rebase replaced the review of the pull request".to_owned(),
                )
            } else if let Some(NotReviewable(why)) = error.downcast_ref::<NotReviewable>() {
                stopped(why.clone())
            } else {
                stopped(format!("{error:#}"))
            }
        }
    }
}

/// The one comment on the issue when a step stops the run.
fn stopped_comment(step: Step, why: &str, progress: &Progress) -> String {
    let what_exists = match (&progress.pull, progress.plan_published) {
        (Some(pull), _) => format!(
            " Pull request #{} is open: {}.",
            pull.target.number, pull.url
        ),
        (None, true) => " The plan is on the issue.".to_owned(),
        (None, false) => String::new(),
    };
    format!(
        "I stopped the autonomous run during {}: {}.{what_exists}",
        step.doing(),
        why.trim_end_matches('.')
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::too_many_lines
    )]

    use std::sync::Mutex;

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::review::ReviewOutcome;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{ChatMessage, Completion, ModelClient, StopReason, Usage};
    use henk_mcp::McpSession;
    use henk_mcp::testing::{FakeServer, echo_behaviour};
    use henk_platform::ReviewTarget;
    use tokio::sync::Notify;

    use super::*;
    use crate::config::Config;
    use crate::coordinator::Submitted;
    use crate::listeners::testing::FakeWriter;
    use crate::plan_tools::tests::FakeIssueWriter;

    const CONFIG: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["o"]
[github]
app_id = 1
installation_id = 2
bot_login = "h[bot]"
[mcp.github]
command = "unused"
[endpoints.m]
provider = "openai"
base_url = "https://x.test/v1"
api_key_env = "UNUSED"
[models.m]
endpoint = "m"
model = "scripted"
[endpoints.r]
provider = "openai"
base_url = "https://x.test/v1"
api_key_env = "UNUSED"
[models.r]
endpoint = "r"
model = "scripted-reviewer"
[planning]
requester_id = 3
[execution]
[review.loop]
[autonomous]
[roles.planner]
model = "m"
[roles.reviewer]
model = "r"
[roles.executor]
model = "m"
requester_id = 3
"#;

    const TICKET: &str = "r-ticket";

    /// The repository's way in, which a ticket run only checks is there.
    struct NoPush;

    fn unused() -> henk_platform::PlatformError {
        henk_platform::PlatformError::Decode("not used by the ticket run".to_owned())
    }

    #[async_trait::async_trait]
    impl henk_platform::push::PushWriter for NoPush {
        async fn pull_facts(
            &self,
            _: &ReviewTarget,
        ) -> Result<henk_platform::push::PullFacts, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn repo_head(
            &self,
            _: &RepoRef,
        ) -> Result<henk_platform::push::RepoHead, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn git_credential(
            &self,
        ) -> Result<Option<henk_platform::push::GitCredential>, henk_platform::PlatformError>
        {
            Ok(None)
        }
        async fn commit_identity(
            &self,
        ) -> Result<henk_platform::push::CommitIdentity, henk_platform::PlatformError> {
            Err(unused())
        }
        fn noreply_host(&self) -> Result<String, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn open_pull_request(
            &self,
            _: &RepoRef,
            _: &henk_platform::push::NewPull,
        ) -> Result<henk_platform::push::OpenedPull, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn user_login(&self, _: u64) -> Result<String, henk_platform::PlatformError> {
            Err(unused())
        }
    }

    struct Fixture {
        app: App,
        issue: Arc<FakeIssueWriter>,
    }

    fn fixture_on(config: &str) -> Fixture {
        let settings = Config::parse(config)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut models: std::collections::BTreeMap<String, Arc<dyn ModelClient>> =
            std::collections::BTreeMap::new();
        for id in ["m", "r"] {
            models.insert(
                id.to_owned(),
                Arc::new(ScriptedClient::new("scripted", [])) as Arc<dyn ModelClient>,
            );
        }
        let issue = Arc::new(FakeIssueWriter::new(
            Platform::GitHub,
            None,
            "Export runs as CSV.",
        ));
        Fixture {
            app: App {
                settings,
                store: Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
                models,
                github: None,
                gitlab: None,
                shutdown: CancellationToken::new(),
                live_runs: crate::liveness::LiveRuns::default(),
                feed: crate::live::Feed::default(),
                cancels: crate::cancel::Cancels::default(),
                workspace_provider: Arc::new(crate::workspace::host::HostProvider),
                own_pushes: crate::push::OwnPushes::default(),
                test_writer: Some(Arc::new(FakeWriter::default())),
                test_session: None,
                test_push_writer: Some(Arc::new(NoPush)),
                test_issue_writer: Some(Arc::clone(&issue) as Arc<dyn IssueWriter>),
                test_ci_reader: None,
            },
            issue,
        }
    }

    fn fixture() -> Fixture {
        fixture_on(CONFIG)
    }

    fn repo() -> RepoRef {
        RepoRef::parse(Platform::GitHub, "o/r").unwrap()
    }

    fn request() -> TicketRequest {
        TicketRequest {
            target: IssueTarget {
                repo: repo(),
                number: 9,
            },
            note: Some("keep it small".to_owned()),
            trigger: "test".to_owned(),
            run: Some(RunId::parse(TICKET).unwrap()),
            requester: Some("github:1234".to_owned()),
        }
    }

    fn limits() -> Limits {
        Limits {
            run: Duration::from_mins(1),
            grace: Duration::from_millis(100),
        }
    }

    fn sha() -> CommitSha {
        CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap()
    }

    fn converged() -> PlanReport {
        PlanReport {
            run: RunId::parse("r-plan").unwrap(),
            planned: true,
            stop: PlanStop::Converged,
            changes: Vec::new(),
        }
    }

    fn opened() -> ExecuteReport {
        ExecuteReport {
            run: RunId::parse("r-exec").unwrap(),
            pull: OpenedPullRequest {
                target: ReviewTarget {
                    repo: repo(),
                    number: 12,
                },
                commit: sha(),
                url: "https://github.com/o/r/pull/12".to_owned(),
            },
        }
    }

    /// A review that passed: nothing in the diff left to review.
    fn passed() -> ReviewReport {
        ReviewReport {
            run: RunId::parse("r-review").unwrap(),
            outcome: Some(ReviewOutcome {
                commit: sha(),
                sessions: Vec::new(),
                open_findings: 0,
                nothing_to_review: true,
                stopped: None,
                not_reviewed: None,
                findings_left: false,
                report: None,
            }),
            summary: None,
        }
    }

    fn failed(what: &'static str, reason: &str) -> anyhow::Error {
        RunFailed {
            what,
            reason: reason.to_owned(),
        }
        .into()
    }

    /// What a scripted plan or execute run does.
    enum Doing<T> {
        Return(anyhow::Result<T>),
        /// Runs until it is stopped, then ends cancelled by whoever stopped it.
        Hang,
        /// Never ends, whatever it is told.
        Ignore,
    }

    /// What the scripted review does.
    enum Reviewing {
        /// Decided as `Decision`, and ended as the result says at once.
        Ends(Decision, anyhow::Result<ReviewReport>),
        /// Started, and never told how it ended unless it is stopped.
        Hangs,
        /// Started, and its sender dropped, as a task that died would.
        Dies,
    }

    /// The stages, scripted by the test.
    struct Scripted {
        plan: Mutex<Option<Doing<PlanReport>>>,
        execute: Mutex<Option<Doing<ExecuteReport>>>,
        review: Mutex<Option<Reviewing>>,
        held: Mutex<Option<oneshot::Sender<anyhow::Result<ReviewReport>>>>,
        plan_request: Mutex<Option<PlanRequest>>,
        execute_request: Mutex<Option<ExecuteRequest>>,
        review_request: Mutex<Option<(ReviewRequest, CommitSha)>>,
        stopped: Mutex<Vec<(RunId, String)>>,
        calls: Mutex<Vec<&'static str>>,
        stop_signal: Notify,
        /// A stage that was stopped ended, after it was told to.
        ended_after_stop: Mutex<bool>,
    }

    impl Scripted {
        fn new(
            plan: Option<Doing<PlanReport>>,
            execute: Option<Doing<ExecuteReport>>,
            review: Option<Reviewing>,
        ) -> Self {
            Self {
                plan: Mutex::new(plan),
                execute: Mutex::new(execute),
                review: Mutex::new(review),
                held: Mutex::new(None),
                plan_request: Mutex::new(None),
                execute_request: Mutex::new(None),
                review_request: Mutex::new(None),
                stopped: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
                stop_signal: Notify::new(),
                ended_after_stop: Mutex::new(false),
            }
        }

        /// Plan, execute and review all go well.
        fn well() -> Self {
            Self::new(
                Some(Doing::Return(Ok(converged()))),
                Some(Doing::Return(Ok(opened()))),
                Some(Reviewing::Ends(Decision::Start, Ok(passed()))),
            )
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        fn stops(&self) -> Vec<(String, String)> {
            self.stopped
                .lock()
                .unwrap()
                .iter()
                .map(|(run, by)| (run.to_string(), by.clone()))
                .collect()
        }

        /// Waits for a stop, then ends cancelled by whoever asked.
        async fn hang<T>(&self) -> anyhow::Result<T> {
            self.stop_signal.notified().await;
            *self.ended_after_stop.lock().unwrap() = true;
            let by = self.stopped.lock().unwrap().last().unwrap().1.clone();
            Err(CancelledBy(by).into())
        }
    }

    #[async_trait::async_trait]
    impl Stages for Scripted {
        async fn plan(&self, request: PlanRequest) -> anyhow::Result<PlanReport> {
            self.calls.lock().unwrap().push("plan");
            *self.plan_request.lock().unwrap() = Some(request);
            let doing = self
                .plan
                .lock()
                .unwrap()
                .take()
                .expect("a plan is scripted");
            match doing {
                Doing::Return(result) => result,
                Doing::Hang => self.hang().await,
                Doing::Ignore => std::future::pending().await,
            }
        }

        async fn execute(&self, request: ExecuteRequest) -> anyhow::Result<ExecuteReport> {
            self.calls.lock().unwrap().push("execute");
            *self.execute_request.lock().unwrap() = Some(request);
            let doing = self
                .execute
                .lock()
                .unwrap()
                .take()
                .expect("an execute run is scripted");
            match doing {
                Doing::Return(result) => result,
                Doing::Hang => self.hang().await,
                Doing::Ignore => std::future::pending().await,
            }
        }

        fn review(
            &self,
            request: ReviewRequest,
            commit: CommitSha,
        ) -> (
            Decision,
            RunId,
            oneshot::Receiver<anyhow::Result<ReviewReport>>,
        ) {
            self.calls.lock().unwrap().push("review");
            *self.review_request.lock().unwrap() = Some((request, commit));
            let (tell, ended) = oneshot::channel();
            let run = RunId::parse("r-review").unwrap();
            let reviewing = self
                .review
                .lock()
                .unwrap()
                .take()
                .expect("a review is scripted");
            match reviewing {
                Reviewing::Ends(decision, result) => {
                    if matches!(decision, Decision::Start | Decision::Supersede) {
                        let _ = tell.send(result);
                    }
                    (decision, run, ended)
                }
                Reviewing::Hangs => {
                    *self.held.lock().unwrap() = Some(tell);
                    (Decision::Start, run, ended)
                }
                Reviewing::Dies => (Decision::Start, run, ended),
            }
        }

        fn stop(&self, run: &RunId, by: String) -> bool {
            self.stopped.lock().unwrap().push((run.clone(), by.clone()));
            if let Some(tell) = self.held.lock().unwrap().take() {
                *self.ended_after_stop.lock().unwrap() = true;
                let _ = tell.send(Err(CancelledBy(by).into()));
            } else {
                self.stop_signal.notify_one();
            }
            true
        }
    }

    /// Runs a ticket run with its token registered, as `submit_ticket` does.
    async fn run(f: &Fixture, stages: &Scripted) -> TicketReport {
        let cancel = f.app.shutdown.child_token();
        let _registered = f
            .app
            .cancels
            .register(RunId::parse(TICKET).unwrap(), cancel.clone());
        run_ticket(&f.app, stages, request(), cancel, limits())
            .await
            .unwrap()
    }

    async fn record(f: &Fixture) -> henk_store::RunRecord {
        f.app
            .store
            .run(&RunId::parse(TICKET).unwrap())
            .await
            .unwrap()
            .unwrap()
    }

    async fn stage_states(f: &Fixture) -> Vec<(Stage, StageState)> {
        f.app
            .store
            .stages(&RunId::parse(TICKET).unwrap())
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.stage, s.state))
            .collect()
    }

    fn comments(f: &Fixture) -> Vec<String> {
        f.issue.comments.lock().unwrap().clone()
    }

    /// Waits, up to four seconds, until `ready` holds.
    async fn until(what: &str, ready: impl Fn() -> bool) {
        for _ in 0..400 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never: {what}");
    }

    /// The comment is the one stop comment: in style, with the run link.
    fn assert_one_stop_comment(f: &Fixture, during: &str, why: &str) -> String {
        let posted = comments(f);
        assert_eq!(posted.len(), 1, "{posted:?}");
        let comment = posted[0].clone();
        assert!(
            comment.starts_with(&format!(
                "I stopped the autonomous run during {during}: {why}"
            )),
            "{comment}"
        );
        assert!(comment.contains(&format!("/runs/{TICKET}")), "{comment}");
        assert!(henk_domain::text::is_in_style(&comment), "{comment}");
        comment
    }

    /// #536: a plan that converged goes to an execute run, whose pull
    /// request goes to a review that passes. Each hands the next what it
    /// needs, the three runs are linked, and nobody is told anything.
    #[tokio::test]
    async fn a_converged_plan_leads_to_an_execute_run_and_a_review_that_passes() {
        let f = fixture();
        let stages = Scripted::well();
        let report = run(&f, &stages).await;

        assert_eq!(report.ending, Ending::Passed);
        assert_eq!(stages.calls(), ["plan", "execute", "review"]);
        let pull = report.pull.unwrap();
        assert_eq!(pull.target.number, 12);

        // The plan: quiet, for the issue, under the ticket run's trigger.
        let plan = stages.plan_request.lock().unwrap().take().unwrap();
        assert_eq!(plan.target.number, 9);
        assert_eq!(plan.note.as_deref(), Some("keep it small"));
        assert_eq!(plan.trigger, "autonomous run r-ticket");
        assert_eq!(plan.requester.as_deref(), Some("github:1234"));
        assert_eq!(plan.comments, Comments::Caller);
        let plan_run = plan.run.unwrap();

        // The execute run: the same issue, quiet, its own run.
        let execute = stages.execute_request.lock().unwrap().take().unwrap();
        assert_eq!(execute.target.number, 9);
        assert_eq!(execute.note.as_deref(), Some("keep it small"));
        assert_eq!(execute.trigger, "autonomous run r-ticket");
        assert_eq!(execute.comments, Comments::Caller);
        let execute_run = execute.run.unwrap();
        assert_ne!(plan_run, execute_run);

        // The review: the pull request it opened, at the commit it pushed.
        let (review, commit) = stages.review_request.lock().unwrap().take().unwrap();
        assert_eq!(review.target.number, 12);
        assert_eq!(commit, sha());
        assert_eq!(review.commit, Some(sha()));
        assert_eq!(review.trigger, "autonomous run r-ticket");
        assert_eq!(review.requester.as_deref(), Some("github:1234"));

        // The three are the ticket run's children, and its stages say so.
        let ticket = RunId::parse(TICKET).unwrap();
        assert_eq!(
            f.app.store.children(&ticket).await.unwrap(),
            [
                (Stage::Plan, plan_run),
                (Stage::Execute, execute_run),
                (Stage::Review, RunId::parse("r-review").unwrap()),
            ]
        );
        assert_eq!(
            stage_states(&f).await,
            [
                (Stage::Requested, StageState::Done),
                (Stage::Started, StageState::Done),
                (Stage::Plan, StageState::Done),
                (Stage::Execute, StageState::Done),
                (Stage::Review, StageState::Done),
                (Stage::Done, StageState::Done),
            ]
        );
        let record = record(&f).await;
        assert_eq!(record.status, RunStatus::Finished);
        assert_eq!(record.kind, RunKind::Ticket);
        assert_eq!(
            record.summary.as_deref(),
            Some("pull request #12 reviewed and passed: https://github.com/o/r/pull/12")
        );
        assert!(comments(&f).is_empty(), "a run that went well says nothing");
    }

    #[tokio::test]
    async fn a_plan_that_fails_stops_the_run_with_one_comment_and_no_execute_run() {
        let f = fixture();
        let stages = Scripted::new(
            Some(Doing::Return(Err(failed(
                "planning",
                "the model ended without writing a plan",
            )))),
            None,
            None,
        );
        let report = run(&f, &stages).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Plan,
                why: "the model ended without writing a plan".to_owned()
            }
        );
        assert_eq!(stages.calls(), ["plan"]);
        let comment =
            assert_one_stop_comment(&f, "planning", "the model ended without writing a plan.");
        assert!(!comment.contains("is on the issue"), "{comment}");
        let record = record(&f).await;
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("the model ended without writing a plan")
        );
        assert_eq!(
            stage_states(&f).await,
            [
                (Stage::Requested, StageState::Done),
                (Stage::Started, StageState::Done),
                (Stage::Plan, StageState::Failed),
                (Stage::Execute, StageState::Skipped),
                (Stage::Review, StageState::Skipped),
                (Stage::Done, StageState::Failed),
            ],
            "the steps never reached say so"
        );
    }

    /// A plan whose review ran out of rounds, time or sessions is on the
    /// issue with its points still open; the run does not build on it.
    #[tokio::test]
    async fn a_plan_review_that_did_not_settle_stops_the_run_before_the_execute_run() {
        for (stop, why) in [
            (
                PlanStop::MaxRounds(3),
                "the plan review did not settle: it reached max_rounds (3)",
            ),
            (
                PlanStop::Timeout("the time limit of 3600s was reached".to_owned()),
                "the plan review did not settle: the time limit of 3600s was reached",
            ),
            (
                PlanStop::SessionFailed("the reviewer did not finish round 2".to_owned()),
                "the plan review did not settle: the reviewer did not finish round 2",
            ),
        ] {
            let f = fixture();
            let stages = Scripted::new(
                Some(Doing::Return(Ok(PlanReport {
                    stop,
                    ..converged()
                }))),
                None,
                None,
            );
            let report = run(&f, &stages).await;
            assert_eq!(
                report.ending,
                Ending::Stopped {
                    step: Step::Plan,
                    why: why.to_owned()
                }
            );
            assert_eq!(stages.calls(), ["plan"], "{why}");
            let comment = assert_one_stop_comment(&f, "planning", &format!("{why}."));
            assert!(comment.contains("The plan is on the issue."), "{comment}");
            assert_eq!(record(&f).await.status, RunStatus::Failed);
        }
    }

    #[tokio::test]
    async fn an_execute_run_that_opens_no_pull_request_stops_the_run() {
        let f = fixture();
        let stages = Scripted::new(
            Some(Doing::Return(Ok(converged()))),
            Some(Doing::Return(Err(failed(
                "execution",
                "the executor changed nothing: JSON export",
            )))),
            None,
        );
        let report = run(&f, &stages).await;

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Execute,
                why: "the executor changed nothing: JSON export".to_owned()
            }
        );
        assert_eq!(stages.calls(), ["plan", "execute"], "no review");
        let comment = assert_one_stop_comment(
            &f,
            "execution",
            "the executor changed nothing: JSON export.",
        );
        assert!(comment.contains("The plan is on the issue."), "{comment}");
        assert_eq!(
            stage_states(&f).await[2..],
            [
                (Stage::Plan, StageState::Done),
                (Stage::Execute, StageState::Failed),
                (Stage::Review, StageState::Skipped),
                (Stage::Done, StageState::Failed),
            ]
        );
    }

    /// A review that leaves its findings, or that did not complete, is no
    /// pass: the run stops and the comment names the pull request.
    #[tokio::test]
    async fn a_review_that_is_no_pass_stops_the_run_with_the_pull_request_named() {
        for (outcome, why) in [
            (
                ReviewOutcome {
                    nothing_to_review: false,
                    findings_left: true,
                    ..passed().outcome.unwrap()
                },
                "1 finding open",
            ),
            (
                ReviewOutcome {
                    nothing_to_review: false,
                    ..passed().outcome.unwrap()
                },
                "Review did not complete",
            ),
        ] {
            let f = fixture();
            let report = ReviewReport {
                outcome: Some(outcome),
                ..passed()
            };
            let stages = Scripted::new(
                Some(Doing::Return(Ok(converged()))),
                Some(Doing::Return(Ok(opened()))),
                Some(Reviewing::Ends(Decision::Start, Ok(report))),
            );
            let ended = run(&f, &stages).await;
            let Ending::Stopped { step, why: said } = &ended.ending else {
                panic!("{:?}", ended.ending)
            };
            assert_eq!(*step, Step::Review);
            let comments = comments(&f);
            assert_eq!(comments.len(), 1, "{comments:?}");
            assert!(
                comments[0].contains(&format!("during the review: {said}")),
                "{why}: {}",
                comments[0]
            );
            assert!(
                comments[0].contains("Pull request #12 is open: https://github.com/o/r/pull/12."),
                "{}",
                comments[0]
            );
            assert!(henk_domain::text::is_in_style(&comments[0]));
            assert_eq!(record(&f).await.status, RunStatus::Failed);
        }
    }

    #[tokio::test]
    async fn a_review_that_cannot_report_stops_the_run() {
        let cases: [(Reviewing, &str); 3] = [
            (
                Reviewing::Ends(
                    Decision::Start,
                    Err(NotReviewable("pull request #12 is a draft".to_owned()).into()),
                ),
                "pull request #12 is a draft",
            ),
            (
                Reviewing::Ends(Decision::Start, Err(anyhow!("the platform is down"))),
                "the platform is down",
            ),
            (Reviewing::Dies, "the review ended without a report"),
        ];
        for (reviewing, why) in cases {
            let f = fixture();
            let stages = Scripted::new(
                Some(Doing::Return(Ok(converged()))),
                Some(Doing::Return(Ok(opened()))),
                Some(reviewing),
            );
            let report = run(&f, &stages).await;
            assert_eq!(
                report.ending,
                Ending::Stopped {
                    step: Step::Review,
                    why: why.to_owned()
                }
            );
            assert_one_stop_comment(&f, "the review", &format!("{why}."));
        }
    }

    /// A person's push replaces the review, or another run already reviews
    /// the pull request: the job of the ticket run is done, and it ends well.
    #[tokio::test]
    async fn a_pull_request_a_person_or_another_run_took_over_hands_the_run_over() {
        let superseded = ReviewReport {
            outcome: Some(ReviewOutcome::superseded(sha())),
            ..passed()
        };
        let cases = [
            (
                Reviewing::Ends(Decision::Start, Err(Superseded.into())),
                "a newer commit or a rebase replaced the review of the pull request",
            ),
            (
                Reviewing::Ends(Decision::Start, Ok(superseded)),
                "a newer commit replaced the review of the pull request",
            ),
            (
                Reviewing::Ends(Decision::Join, Err(anyhow!("never read"))),
                "review run r-review already covers the pull request",
            ),
            (
                Reviewing::Ends(Decision::After, Err(anyhow!("never read"))),
                "review run r-review already covers the pull request",
            ),
        ];
        for (reviewing, why) in cases {
            let f = fixture();
            let stages = Scripted::new(
                Some(Doing::Return(Ok(converged()))),
                Some(Doing::Return(Ok(opened()))),
                Some(reviewing),
            );
            let report = run(&f, &stages).await;
            assert_eq!(report.ending, Ending::HandedOver(why.to_owned()));
            assert!(report.ending.is_well());
            assert!(comments(&f).is_empty(), "{why}");
            let record = record(&f).await;
            assert_eq!(record.status, RunStatus::Finished, "{why}");
            assert_eq!(
                record.summary.as_deref(),
                Some(format!("handed over: {why}").as_str())
            );
        }
    }

    /// A person cancelling the ticket run stops the stage it is in as they
    /// would have, and the issue hears it once.
    #[tokio::test]
    async fn a_person_cancelling_the_run_stops_its_stage_and_ends_it_cancelled() {
        for in_review in [false, true] {
            let f = fixture();
            let stages = if in_review {
                Scripted::new(
                    Some(Doing::Return(Ok(converged()))),
                    Some(Doing::Return(Ok(opened()))),
                    Some(Reviewing::Hangs),
                )
            } else {
                Scripted::new(Some(Doing::Hang), None, None)
            };
            let waiting = if in_review { "review" } else { "plan" };
            let ticket = RunId::parse(TICKET).unwrap();
            let (report, ()) = tokio::join!(run(&f, &stages), async {
                until("the stage started", || stages.calls().contains(&waiting)).await;
                assert!(f.app.cancels.cancel(&ticket, "github:1234".to_owned()));
            });

            assert_eq!(report.ending, Ending::Cancelled("github:1234".to_owned()));
            let stopped = stages.stops();
            assert_eq!(stopped.len(), 1, "{stopped:?}");
            assert_eq!(stopped[0].1, "github:1234");
            assert!(
                *stages.ended_after_stop.lock().unwrap(),
                "it was waited for"
            );
            let comments = comments(&f);
            assert_eq!(comments.len(), 1, "{comments:?}");
            assert!(
                comments[0].starts_with("Cancelled from the dashboard by GitHub account 1234."),
                "{}",
                comments[0]
            );
            assert_eq!(record(&f).await.status, RunStatus::Cancelled);
        }
    }

    /// A person who cancels the plan run itself, not the ticket run, ends
    /// the ticket run cancelled too: the stage was quiet, so it says so.
    #[tokio::test]
    async fn a_person_cancelling_a_stage_directly_ends_the_run_cancelled() {
        let f = fixture();
        let stages = Scripted::new(
            Some(Doing::Return(Err(
                CancelledBy("mcp:claude".to_owned()).into()
            ))),
            None,
            None,
        );
        let report = run(&f, &stages).await;
        assert_eq!(report.ending, Ending::Cancelled("mcp:claude".to_owned()));
        assert_eq!(stages.calls(), ["plan"]);
        let comments = comments(&f);
        assert_eq!(comments.len(), 1, "{comments:?}");
        assert!(
            comments[0].starts_with("Cancelled over MCP by client claude."),
            "{}",
            comments[0]
        );
        assert_eq!(record(&f).await.status, RunStatus::Cancelled);
    }

    /// Henk stopping is no failure and says nothing; a stage that does not
    /// end when told is left after the grace.
    #[tokio::test]
    async fn a_shutdown_interrupts_the_run_without_a_word_and_leaves_a_stage_that_ignores_it() {
        let f = fixture();
        let stages = Scripted::new(Some(Doing::Ignore), None, None);
        let (report, ()) = tokio::join!(run(&f, &stages), async {
            until("the plan started", || stages.calls().contains(&"plan")).await;
            f.app.shutdown.cancel();
        });
        assert_eq!(report.ending, Ending::Interrupted);
        assert!(stages.stops().is_empty(), "the shutdown reached it itself");
        assert!(comments(&f).is_empty());
        let record = record(&f).await;
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("interrupted"));
    }

    /// The whole run has a time limit: the stage in flight is cancelled by
    /// the limit, not by a person, waited for, and the run says why.
    #[tokio::test]
    async fn the_whole_run_has_a_time_limit_and_the_stage_is_waited_for() {
        let f = fixture();
        let stages = Scripted::new(
            Some(Doing::Return(Ok(converged()))),
            Some(Doing::Hang),
            None,
        );
        let cancel = f.app.shutdown.child_token();
        let _registered = f
            .app
            .cancels
            .register(RunId::parse(TICKET).unwrap(), cancel.clone());
        let limits = Limits {
            run: Duration::from_secs(1),
            grace: Duration::from_secs(5),
        };
        let report = run_ticket(&f.app, &stages, request(), cancel, limits)
            .await
            .unwrap();

        assert_eq!(
            report.ending,
            Ending::Stopped {
                step: Step::Execute,
                why: "the time limit of 1s for the whole run was reached".to_owned()
            }
        );
        let stopped = stages.stops();
        assert_eq!(stopped.len(), 1, "{stopped:?}");
        assert_eq!(stopped[0].1, LIMIT_BY);
        assert!(
            *stages.ended_after_stop.lock().unwrap(),
            "it was waited for"
        );
        assert_one_stop_comment(
            &f,
            "execution",
            "the time limit of 1s for the whole run was reached.",
        );
        assert_eq!(record(&f).await.status, RunStatus::Failed);
    }

    /// Cancelled before a step starts, no stage runs.
    #[tokio::test]
    async fn a_run_cancelled_before_its_first_step_starts_no_stage() {
        let f = fixture();
        let stages = Scripted::new(None, None, None);
        let cancel = f.app.shutdown.child_token();
        let ticket = RunId::parse(TICKET).unwrap();
        let _registered = f.app.cancels.register(ticket.clone(), cancel.clone());
        assert!(f.app.cancels.cancel(&ticket, "github:7".to_owned()));
        let report = run_ticket(&f.app, &stages, request(), cancel, limits())
            .await
            .unwrap();
        assert_eq!(report.ending, Ending::Cancelled("github:7".to_owned()));
        assert!(stages.calls().is_empty());
        assert_eq!(record(&f).await.status, RunStatus::Cancelled);
    }

    /// Something a stage needs is missing: the run fails with the reason,
    /// before any stage, and the issue is not told: it is for whoever runs Henk.
    #[tokio::test]
    async fn a_run_that_cannot_start_fails_with_the_reason_and_no_comment() {
        let mut f = fixture();
        f.app.models.remove("r");
        let stages = Scripted::new(None, None, None);
        let report = run(&f, &stages).await;
        assert!(matches!(
            report.ending,
            Ending::Stopped {
                step: Step::Plan,
                ..
            }
        ));
        assert!(stages.calls().is_empty());
        assert!(comments(&f).is_empty());
        let record = record(&f).await;
        assert_eq!(record.status, RunStatus::Failed);
        assert!(
            record.error.as_deref().is_some_and(|e| e.contains('r')),
            "{record:?}"
        );
        assert_eq!(
            stage_states(&f).await[2..],
            [
                (Stage::Plan, StageState::Skipped),
                (Stage::Execute, StageState::Skipped),
                (Stage::Review, StageState::Skipped),
                (Stage::Done, StageState::Failed),
            ]
        );
    }

    /// Before any run exists: not configured, a closed issue, a pull request.
    #[tokio::test]
    async fn without_autonomous_a_closed_issue_or_a_pull_request_makes_no_run() {
        let ticket = RunId::parse(TICKET).unwrap();
        let stages = Scripted::new(None, None, None);

        let f = fixture_on(&CONFIG.replace("[autonomous]\n", ""));
        let error = run_ticket(
            &f.app,
            &stages,
            request(),
            CancellationToken::new(),
            limits(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "autonomous runs are not configured");
        assert!(f.app.store.run(&ticket).await.unwrap().is_none());

        let f = fixture();
        f.issue.issue.lock().unwrap().open = false;
        let error = run_ticket(
            &f.app,
            &stages,
            request(),
            CancellationToken::new(),
            limits(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("is closed"), "{error}");
        assert!(f.app.store.run(&ticket).await.unwrap().is_none());

        let f = fixture();
        f.issue.issue.lock().unwrap().is_pull_request = true;
        let error = run_ticket(
            &f.app,
            &stages,
            request(),
            CancellationToken::new(),
            limits(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("is a pull request"), "{error}");
        assert!(f.app.store.run(&ticket).await.unwrap().is_none());
        assert!(stages.calls().is_empty());
    }

    /// A fixture whose planner never answers in time, so a ticket run stays in
    /// its first step until it is cancelled.
    async fn hanging() -> Fixture {
        let mut f = fixture();
        let server = FakeServer::new(
            vec![FakeServer::tool("issue_read", "", &["owner", "repo"])],
            echo_behaviour(),
        );
        let session: Arc<dyn McpSession> = Arc::new(server.connect("github").await);
        f.app.test_session = Some(session);
        let answer = Completion {
            message: ChatMessage::assistant("Plan written."),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        };
        let slow =
            ScriptedClient::new("scripted", [Ok(answer)]).with_delay(Duration::from_secs(30));
        f.app.models.insert("m".to_owned(), Arc::new(slow));
        f
    }

    /// #536: an issue has one ticket run at a time, a second request joins
    /// it, another issue is its own, and a cancel from the dashboard stops
    /// the plan it is in, says so once and frees the issue.
    #[tokio::test]
    async fn an_issue_has_one_ticket_run_at_a_time_and_a_cancel_frees_it() {
        let f = hanging().await;
        let issue = Arc::clone(&f.issue);
        let app = Arc::new(f.app);
        let coordinator = Arc::new(Coordinator::new(Arc::clone(&app)));
        let target = request().target;
        let ask = |target: IssueTarget| {
            coordinator
                .submit_ticket(target, None, "test".to_owned(), Some("github:1".to_owned()))
                .unwrap()
        };

        let Submitted::Started(first) = ask(target.clone()) else {
            panic!("the first request starts a run")
        };
        let mut plan = None;
        for _ in 0..400 {
            if let Some((_, run)) = app.store.children(&first).await.unwrap().into_iter().next() {
                plan = Some(run);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let plan = plan.expect("the ticket run started its plan");
        assert_eq!(coordinator.tracked_tickets(), 1);
        assert_eq!(
            ask(target.clone()),
            Submitted::Joined(first.clone()),
            "one at a time"
        );
        assert_eq!(coordinator.tracked_tickets(), 1);

        assert!(coordinator.cancel(&first, "github:1234".to_owned()));
        until("the ticket run ended", || {
            coordinator.tracked_tickets() == 0
        })
        .await;
        let ticket = app.store.run(&first).await.unwrap().unwrap();
        assert_eq!(ticket.status, RunStatus::Cancelled);
        let plan = app.store.run(&plan).await.unwrap().unwrap();
        assert_eq!(plan.status, RunStatus::Cancelled, "its stage was stopped");
        let comments = issue.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1, "{comments:?}");
        assert!(
            comments[0].starts_with("Cancelled from the dashboard by GitHub account 1234."),
            "{}",
            comments[0]
        );

        // Free again: a request starts a run of its own.
        let Submitted::Started(second) = ask(target) else {
            panic!("a cancelled issue starts afresh")
        };
        assert_ne!(second, first);
        assert!(coordinator.cancel(&second, "github:1234".to_owned()));
        until("the second ended", || coordinator.tracked_tickets() == 0).await;
        app.shutdown.cancel();
    }

    #[test]
    fn the_limits_come_from_autonomous() {
        let f = fixture();
        let limits = Limits::from_settings(&f.app.settings).unwrap();
        assert_eq!(limits.run, Duration::from_hours(2));
        let f = fixture_on(&CONFIG.replace("[autonomous]\n", ""));
        assert!(Limits::from_settings(&f.app.settings).is_err());
    }
}
