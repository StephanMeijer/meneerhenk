//! The reviewer↔executor loop (#284), which is the review (#309): a reviewer
//! reviews the pull request, an executor fixes what holds and Henk pushes it to the
//! pull request's branch, and each resumes its own conversation the next
//! round, until the reviewer finds nothing more or the rounds run out.
//!
//! Both work in one workspace at the pull request's head, taking turns:
//! after a push the workspace holds the new head, so nothing has to move.
//! The reviewer's view of it is never exported; only the executor's changes,
//! checked against the push rules (§3.5), become a commit.
//!
//! A pull request the loop may not push to (a fork, the default or a
//! protected branch) or whose branch moved after the review was queued is
//! reviewed by the reviewer alone (#307): one round at the reviewed commit,
//! no executor, nothing pushed, and its findings stand, in the check.
//!
//! A pull request merged before the loop starts, or while it works, ends
//! it (#413): no workspace or no further session, and nothing more pushed.
//!
//! Interim until #285 and #286: the reviewer's findings and the executor's
//! report are plain text, and the loop ends on `NO FINDINGS`, at
//! `max_rounds`, or at the first round that cannot go on.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, StopCause, ToolSet, prompts};
use henk_domain::commit::FIX_SUBJECT;
use henk_domain::diff::ReviewDiff;
use henk_domain::identity::RequesterId;
use henk_domain::push::push_refusal;
use henk_domain::review::{CommitSha, SessionName, SessionOutcome, SessionResult};
use henk_domain::review_loop::{LoopStop, LoopVerdict};
use henk_domain::role::Role;
use henk_domain::run::RunId;
use henk_domain::workspace::Profile;
use henk_llm::{CacheTtl, ChatMessage};
use henk_platform::ReviewTarget;
use henk_platform::push::{GitCredential, PullFacts, PushWriter};
use henk_session::{ResumableSession, RoundOutcome, cache_ttl_for};
use henk_store::{Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::App;
use crate::cancel::is_cancelled;
use crate::config::LoopConfig;
use crate::diff_tools::{DiffFiles, GetFileDiff, ListChangedFiles};
use crate::edit_tools::{EditContext, EditState, edit_tools};
use crate::git::{Checkout, ScratchDir, git_command};
use crate::loop_tools::{
    FinishRound, GiveVerdict, Handoff, HandoffState, ListFindings, ReportFinding,
    executor_continuation, finding_text, reviewer_continuation, verdict_text,
};
use crate::push::{MergedMeanwhile, PushTo, Pusher, stop_if_cancelled};
use crate::roles::equip_and_grant;
use crate::stages;
use crate::workspace::{Workspace, checked_changeset, never_exported};

/// The most of a pushed commit's patch the reviewer is shown in its resume
/// message; the rest it reads in the workspace.
const MAX_PATCH_CHARS: usize = 40_000;

/// The pull request one loop works on.
#[derive(Clone, Copy)]
pub(crate) struct LoopTarget<'a> {
    pub(crate) app: &'a App,
    pub(crate) target: &'a ReviewTarget,
    pub(crate) commit: &'a CommitSha,
    pub(crate) run: &'a RunId,
    /// Who asked, when Henk knows them by a stable id: the commits name
    /// them, and nobody else (#399).
    pub(crate) requester: Option<&'a RequesterId>,
    /// The last commit the loop pushed, set the moment the push is done, so
    /// however the review then ends, the pull request's new head gets its
    /// check.
    pub(crate) pushed_head: &'a Mutex<Option<CommitSha>>,
}

/// How a loop ended, for the review's summary.
#[derive(Debug)]
pub(crate) struct LoopReport {
    /// The reviewer and the executor, as the summary counts sessions.
    pub(crate) results: Vec<SessionResult>,
    /// For the check: rounds, commits pushed and why it stopped, then the
    /// findings that still stand.
    pub(crate) report: String,
    /// The findings that ended unsettled or won't fix: what still stands.
    pub(crate) open: usize,
    /// It ran out of rounds with the reviewer's last findings still open.
    pub(crate) findings_left: bool,
    /// The reviewer reviewed alone, on a pull request the loop may not push
    /// to (#307).
    pub(crate) review_only: bool,
    /// The pull request was merged while the loop worked (#413).
    pub(crate) merged: bool,
}

/// The pull request is merged already, so the loop does not start: no
/// workspace opens and no session runs (#413).
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the pull request is merged")]
pub(crate) struct AlreadyMerged;

/// Runs the loop on `diff`. A pull request the loop may not push to, or
/// whose branch moved, gets the reviewer alone (#307). A loop that cannot get
/// a workspace fails before any session starts; once it runs, it ends with a
/// report of why it stopped, and only a cancel or a store failure is an
/// error.
pub(crate) async fn run_loop(
    on: LoopTarget<'_>,
    diff: Arc<ReviewDiff>,
    cancel: &CancellationToken,
) -> anyhow::Result<LoopReport> {
    let LoopTarget {
        app,
        target,
        commit,
        run,
        ..
    } = on;
    let config = app
        .settings
        .review
        .r#loop
        .as_ref()
        .ok_or_else(|| anyhow!("the review loop is not configured"))?;
    let store = &*app.store;
    let writer = app.push_writer(&target.repo)?;
    let facts = writer
        .pull_facts(target)
        .await
        .context("reading the pull request")?;
    // Merged while the review waited: nothing is left to review (#413).
    if facts.push.merged {
        return Err(AlreadyMerged.into());
    }
    // Why the reviewer reviews alone, if it does: it is never pushed to.
    let alone = push_refusal(&facts.push).or_else(|| {
        (facts.head != *commit).then(|| {
            format!(
                "the branch moved to {} after this review was queued",
                facts.head.short()
            )
        })
    });
    if let Some(why) = &alone {
        info!(run = %run, %why, "the reviewer reviews alone");
    }
    let credential = writer
        .git_credential()
        .await
        .context("getting a credential for git")?;
    let profile = app.settings.workspace.profile_for(&target.repo.path());
    // The run's time counts from here: a wait for a sandbox host is part of
    // it (#403).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(config.run_timeout_secs);
    stages::mark(store, run, Stage::Checkout, StageState::Running, "").await;
    let opening = open_workspace(
        app,
        run,
        &facts,
        alone.is_some().then_some(commit),
        credential.clone(),
        (profile, deadline),
    );
    // A setup can take minutes; a cancel does not wait for it. The
    // workspace dropped mid-setup destroys itself.
    let opened = tokio::select! {
        opened = opening => opened,
        () = cancel.cancelled() => return Err(crate::cancel::Cancelled.into()),
    };
    let workspace = match opened {
        Ok(workspace) => workspace,
        Err(error) => {
            let why = format!("{error:#}");
            stages::mark(store, run, Stage::Checkout, StageState::Failed, &why).await;
            return Err(anyhow!("the review loop needs a workspace: {why}"));
        }
    };
    let ready = format!("ready at {}", commit.short());
    stages::mark(store, run, Stage::Checkout, StageState::Done, ready).await;
    if let Some(why) = &alone {
        let skipped = format!("nothing is pushed: {why}");
        stages::mark(store, run, Stage::Push, StageState::Skipped, skipped).await;
    }

    let mut rounds = Rounds::new(on, config, profile, &workspace, &diff, writer)?
        .alone(alone)
        .until(deadline);
    // Closed on every path; a run future that is dropped drops it instead,
    // and every backend destroys itself then too.
    let ended = rounds.run(facts, &diff, cancel).await;
    rounds.reviewer.finish(store, run).await;
    if let Some(executor) = &mut rounds.executor {
        executor.finish(store, run).await;
    }
    workspace.close().await;
    rounds.conclude(ended).await
}

/// Checks out the pull request, imports it into a workspace that records
/// every command on the run, and sets it up as the profile says. The loop
/// clones the branch at its head, which it pushes to; the reviewer alone
/// gets `reviewed`, fetched by its sha, which a fork or a branch that moved
/// on still has.
async fn open_workspace(
    app: &App,
    run: &RunId,
    facts: &PullFacts,
    reviewed: Option<&CommitSha>,
    credential: Option<GitCredential>,
    (profile, deadline): (&Profile, tokio::time::Instant),
) -> anyhow::Result<Arc<dyn Workspace>> {
    let dir =
        ScratchDir::new(&format!("henk-loop-{run}")).context("making the checkout directory")?;
    let checkout = match reviewed {
        Some(commit) => Checkout::fetch_at(dir, &facts.remote, commit, credential).await,
        None => {
            Checkout::clone_at(
                dir,
                &facts.remote,
                &facts.push.head_ref,
                &facts.head,
                credential,
            )
            .await
        }
    }
    .context("checking out the pull request")?;
    crate::workspace::open_prepared(app, run, &checkout, profile, Some(deadline)).await
}

/// The loop's state across rounds.
struct Rounds<'a> {
    on: LoopTarget<'a>,
    config: &'a LoopConfig,
    workspace: Arc<dyn Workspace>,
    fixing: Arc<EditContext>,
    handoff: Arc<Handoff>,
    reviewer: ResumableSession,
    /// None when the reviewer reviews alone.
    executor: Option<ResumableSession>,
    /// Why the reviewer reviews alone, if it does (#307).
    alone: Option<String>,
    writer: Arc<dyn PushWriter>,
    /// The round under way, or the last one.
    round: u32,
    /// The commits pushed, in order.
    pushed: Vec<String>,
    /// When the whole run must have ended (#286).
    deadline: tokio::time::Instant,
    /// The reviewer's limit per round, from `[roles.reviewer]`.
    reviewer_round: Duration,
    /// The executor's limit per round, from `[roles.executor]`.
    executor_round: Duration,
}

impl<'a> Rounds<'a> {
    fn new(
        on: LoopTarget<'a>,
        config: &'a LoopConfig,
        profile: &Profile,
        workspace: &Arc<dyn Workspace>,
        diff: &Arc<ReviewDiff>,
        writer: Arc<dyn PushWriter>,
    ) -> anyhow::Result<Self> {
        let app = on.app;
        let (reviewer, address) = loop_roles(&app.settings)?;
        let command_limit = Duration::from_secs(profile.limits.command_secs);
        let bash = profile.backend.runs_model_commands();
        let reference = format!("#{}", on.target.number);
        let repo = on.target.repo.path();
        let settings = &app.settings;
        let reviewer_round = reviewer.timeout_secs(Role::Reviewer);

        let reviewer_model = app.model(&reviewer.model)?;
        let handoff = Arc::new(Handoff {
            run: on.run.clone(),
            store: Arc::clone(&app.store),
            reviewer_model: reviewer_model.model().to_owned(),
            state: Mutex::new(HandoffState::default()),
        });
        let mut reviewer_system = format!(
            "{}\n\n{}",
            prompts::PERSONA,
            prompts::render(
                prompts::LOOP_REVIEWER,
                &[("ref", &reference), ("repo", &repo)]
            )
        );
        let reviewer_tools = equip_and_grant(
            Role::Reviewer,
            &mut reviewer_system,
            reviewer_tools(workspace, diff, &handoff, bash.then_some(command_limit)),
            app.settings.skills.select(&reviewer.skills),
            &reviewer.tools(Role::Reviewer),
        );

        // The executor edits, and runs the repository's checks with bash
        // where it has it (#396).
        let fixing = Arc::new(EditContext {
            workspace: Arc::clone(workspace),
            command_timeout: command_limit,
            max_changed_files: address.max_changed_files,
            // Told of bash only when it has it: the backend runs model
            // commands and [roles.executor] tools grant it.
            bash: bash && Role::Executor.grants(&address.tools(), "bash"),
            state: Mutex::new(EditState::default()),
        });
        let (executor_system, executor_tools) =
            executor_setup(app, address, &fixing, &handoff, &reference, &repo);

        let reviewer_turns = reviewer.max_turns(Role::Reviewer);
        let reviewer_limits = round_limits(
            settings,
            reviewer_turns,
            reviewer_round,
            address.timeout_secs,
        );
        let executor_limits = round_limits(
            settings,
            address.max_turns,
            address.timeout_secs,
            reviewer_round,
        );
        Ok(Self {
            on,
            config,
            workspace: Arc::clone(workspace),
            reviewer: ResumableSession::new(
                Role::Reviewer,
                reviewer_model,
                reviewer_system,
                reviewer_tools,
                reviewer_limits,
            )
            .with_continuation(reviewer_continuation(Arc::clone(&handoff))),
            executor: Some(
                ResumableSession::new(
                    Role::Executor,
                    app.model(&address.model)?,
                    executor_system,
                    executor_tools,
                    executor_limits,
                )
                .with_continuation(executor_continuation(Arc::clone(&handoff))),
            ),
            alone: None,
            fixing,
            handoff,
            writer,
            round: 0,
            pushed: Vec::new(),
            deadline: tokio::time::Instant::now() + Duration::from_secs(config.run_timeout_secs),
            reviewer_round: Duration::from_secs(reviewer_round),
            executor_round: Duration::from_secs(address.timeout_secs),
        })
    }

    /// Gives every finding its one final verdict, whatever ended the loop
    /// (#285), and sums the loop up for the review.
    async fn conclude(&self, ended: anyhow::Result<LoopStop>) -> anyhow::Result<LoopReport> {
        let (store, run) = (&*self.on.app.store, self.on.run);
        // A cancel just before or during a push is a cancel like one inside a
        // session: the review sees its token and ends the way that says.
        let ended = match ended {
            Err(error) if is_cancelled(&error) => Ok(LoopStop::Cancelled),
            other => other,
        };
        // Every finding ends with one verdict, whatever ended the loop (#285).
        let why = match &ended {
            Ok(LoopStop::ReviewOnly(why)) => format!("not fixed: {why}"),
            Ok(stop) => format!("the loop stopped: {stop}"),
            Err(error) => format!("the loop stopped: {error:#}"),
        };
        let settled = self
            .handoff
            .with_ledger(|ledger| {
                let mut ids = ledger.unpushed(&why);
                ids.extend(ledger.close_all(&why));
                ids
            })
            .unwrap_or_default();
        self.handoff.record_verdicts(&settled).await;
        let stop = ended?;
        if let Err(error) = store.end_loop(run, stop.as_str(), self.round).await {
            tracing::warn!(%error, "could not record why the loop stopped");
        }
        // A cancel is a skip, as for every other stage of a cancelled run,
        // not a failure of the loop.
        let cancelled = matches!(stop, LoopStop::Cancelled);
        let failed = !cancelled && !stop.ended_well();
        let ledger = self.handoff.ledger();
        let open = ledger.standing();

        let pushed = self.pushed.len();
        let found = ledger.iter().count();
        let line = format!(
            "The review loop ran {} {}, settled {found} {}, pushed {pushed} {} and stopped: {stop}.",
            self.round,
            if self.round == 1 { "round" } else { "rounds" },
            if found == 1 { "finding" } else { "findings" },
            if pushed == 1 { "commit" } else { "commits" },
        );
        info!(run = %run, rounds = self.round, pushed, %stop, "review loop ended");
        let _ = store.event(run, "info", &line).await;
        let state = if cancelled {
            StageState::Skipped
        } else if failed {
            StageState::Failed
        } else {
            StageState::Done
        };
        stages::mark(store, run, Stage::Rounds, state, &line).await;
        let outcome = if failed {
            SessionOutcome::Dropped
        } else {
            SessionOutcome::Finished
        };
        let sessions: &[&str] = if self.executor.is_some() {
            &["executor", "reviewer"]
        } else {
            &["reviewer"]
        };
        let results = sessions
            .iter()
            .map(|name| SessionResult {
                session: SessionName::new(*name),
                outcome,
            })
            .collect();
        Ok(LoopReport {
            results,
            report: ledger.check_report(&line),
            open,
            findings_left: stop.left_findings_open(),
            review_only: self.alone.is_some(),
            merged: stop == LoopStop::Merged,
        })
    }

    /// Takes rounds until one says to stop, and returns why.
    async fn run(
        &mut self,
        mut facts: PullFacts,
        diff: &ReviewDiff,
        cancel: &CancellationToken,
    ) -> anyhow::Result<LoopStop> {
        let (store, run) = (&*self.on.app.store, self.on.run);
        let first = opening(self.on.target, self.on.commit, diff);
        let mut message = first.clone();
        while self.round < self.config.max_rounds {
            // A round counts once the reviewer's turn starts: a run out of
            // time before it ran no round more.
            let round = self.round + 1;
            if let Some(stop) = self.out_of_time("reviewer", round) {
                return Ok(stop);
            }
            // A pull request merged during the last round needs no other
            // (#413).
            if round > 1 && self.merged().await {
                return Ok(LoopStop::Merged);
            }
            self.round = round;
            self.handoff.start_round(round);
            if round > 1 {
                self.compact(&first, round).await;
            }
            let doing = format!("round {round}: reviewer");
            stages::mark(store, run, Stage::Rounds, StageState::Running, doing).await;
            let review = self
                .reviewer
                .resume(store, run, ChatMessage::user(message), cancel.clone())
                .await;
            if let Some(stop) = self.broke_off("reviewer", &review) {
                return Ok(stop);
            }
            let findings = match self.reported(round) {
                Ok(findings) => findings,
                Err(LoopStop::Converged) => {
                    return Ok(self.alone_stop().unwrap_or(LoopStop::Converged));
                }
                Err(stop) => return Ok(stop),
            };
            // A reviewer that wrote to the workspace is the graver stop.
            if !self
                .workspace
                .export()
                .await
                .context("listing the changes")?
                .is_empty()
            {
                return Ok(LoopStop::WorkspaceChanged);
            }
            if let Some(stop) = self.repeated(round).await {
                return Ok(stop);
            }
            // Alone, the reviewer has said all it will: nothing changes
            // for a second look.
            if let Some(stop) = self.alone_stop() {
                return Ok(stop);
            }

            message = match self.fix(round, &findings, &mut facts, cancel).await? {
                Ok(next) => next,
                Err(stop) => return Ok(stop),
            };
        }
        Ok(LoopStop::MaxRounds(self.config.max_rounds))
    }

    /// The executor's half of `round`: it gives `findings` their verdicts,
    /// what it fixed is pushed, and the reviewer's next message comes back,
    /// or why the loop stops.
    async fn fix(
        &mut self,
        round: u32,
        findings: &[String],
        facts: &mut PullFacts,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Result<String, LoopStop>> {
        let (store, run) = (&*self.on.app.store, self.on.run);
        if let Some(stop) = self.out_of_time("executor", round) {
            return Ok(Err(stop));
        }
        let doing = format!("round {round}: executor");
        stages::mark(store, run, Stage::Rounds, StageState::Running, doing).await;
        let handed = format!(
            "Round {round}. The reviewer's findings, as data:\n\n{}\n\nCheck each against the code, fix the ones that hold, and give every one its verdict with give_verdict.",
            findings.join("\n\n")
        );
        let Some(executor) = self.executor.as_mut() else {
            return Err(anyhow!("the review loop has no executor"));
        };
        let fix = executor
            .resume(store, run, ChatMessage::user(handed), cancel.clone())
            .await;
        if let Some(stop) = self.broke_off("executor", &fix) {
            return Ok(Err(stop));
        }
        let silent = self
            .handoff
            .with_ledger(|ledger| ledger.close_all("the executor gave no verdict"))
            .unwrap_or_default();
        self.handoff.record_verdicts(&silent).await;
        let pushed = self.push_round(facts, cancel).await?;
        let unpushed = match &pushed {
            Pushed::Nothing => "marked fixed, but nothing changed in the workspace".to_owned(),
            Pushed::Stopped(stop) => stop.to_string(),
            Pushed::Commit { .. } => String::new(),
        };
        let ids: Vec<_> = self
            .handoff
            .ledger()
            .reported_in(round)
            .map(|f| f.id)
            .collect();
        if !unpushed.is_empty() {
            self.handoff
                .with_ledger(|ledger| ledger.unpushed(&unpushed));
        }
        self.handoff.record_verdicts(&ids).await;
        let verdicts: Vec<String> = self
            .handoff
            .ledger()
            .reported_in(round)
            .map(verdict_text)
            .collect();
        let verdicts = verdicts.join("\n");
        let contest = "To contest a rejection, once, report the finding again with new evidence and name it in reopens.";
        let next = match pushed {
            Pushed::Nothing => format!(
                "Round {}. The executor pushed nothing this round. Their verdicts:\n\n{verdicts}\n\n{contest} Review again.",
                round + 1
            ),
            Pushed::Commit { sha, patch } => format!(
                "Round {}. The executor pushed commit {sha} to the branch, and your workspace is at it now. What it changed:\n\n```diff\n{patch}\n```\n\nTheir verdicts:\n\n{verdicts}\n\n{contest} Review again.",
                round + 1
            ),
            Pushed::Stopped(stop) => return Ok(Err(stop)),
        };
        Ok(Ok(next))
    }

    /// Keeps both conversations within their budget: what the earlier
    /// rounds came to, in place of all but the last round (#286). The
    /// reviewer's summary starts with its task, `opening`; the executor never
    /// had that task or its tools, so its summary is the findings and the
    /// commits alone.
    /// A compaction is a last resort: it is on the run, with the round,
    /// since the prompt cache is written again from there (#330).
    async fn compact(&mut self, opening: &str, round: u32) {
        let ledger = self.handoff.ledger();
        let commits = if self.pushed.is_empty() {
            "none".to_owned()
        } else {
            self.pushed.join(", ")
        };
        let earlier = format!(
            "The findings so far and their verdicts:\n{}\n\nCommits pushed: {commits}.",
            ledger.summary(round)
        );
        let mut dropped = Vec::new();
        dropped.extend(
            self.reviewer
                .compact_rounds(&format!("{opening}\n\n{earlier}"), 1),
        );
        if let Some(executor) = &mut self.executor {
            dropped.extend(executor.compact_rounds(&earlier, 1));
        }
        for rounds in dropped {
            let _ = self
                .on
                .app
                .store
                .event(self.on.run, "warn", &rounds.note())
                .await;
        }
    }

    /// The timeout stop when the run's time is up; otherwise gives the
    /// next session's round what is left of it, at most a round's limit.
    fn out_of_time(&mut self, who: &str, round: u32) -> Option<LoopStop> {
        let left = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Some(LoopStop::Timeout(format!(
                "the run's time limit of {}s was reached before the {who}'s turn in round {round}",
                self.config.run_timeout_secs
            )));
        }
        let (session, round_limit) = if who == "reviewer" {
            (Some(&mut self.reviewer), self.reviewer_round)
        } else {
            (self.executor.as_mut(), self.executor_round)
        };
        let limit = left.min(round_limit);
        if let Some(session) = session {
            session.set_timeout(limit);
        }
        None
    }

    /// The run's time limit ends at `deadline`, taken before its workspace
    /// opened, so a wait for one counts (#403).
    fn until(mut self, deadline: tokio::time::Instant) -> Self {
        self.deadline = deadline;
        self
    }

    /// Whether the pull request is merged now (#413). A read that fails
    /// says no: the push reads it again before anything is pushed.
    async fn merged(&self) -> bool {
        match self.writer.pull_facts(self.on.target).await {
            Ok(facts) => facts.push.merged,
            Err(error) => {
                warn!(%error, "could not read whether the pull request is merged");
                false
            }
        }
    }

    /// The reviewer reviews alone, without an executor, when `why` says why.
    fn alone(mut self, why: Option<String>) -> Self {
        if why.is_some() {
            // Alone, the reviewer waits for nobody: each turn refreshes the
            // short cache.
            self.executor = None;
            self.reviewer.set_cache_ttl(CacheTtl::Short);
        }
        self.alone = why;
        self
    }

    /// The stop for a reviewer that reviews alone, once its round is done.
    fn alone_stop(&self) -> Option<LoopStop> {
        self.alone.clone().map(LoopStop::ReviewOnly)
    }

    /// The stop when a finding of `round` repeats one the executor fixed:
    /// the two are going in circles (#286). The repeat stays unsettled.
    async fn repeated(&self, round: u32) -> Option<LoopStop> {
        let ledger = self.handoff.ledger();
        let (finding, of) = ledger
            .reported_in(round)
            .find_map(|f| ledger.repeat_of(&f.report).map(|of| (f.id, of)))?;
        let commit = match ledger.get(of).and_then(|f| f.verdict.as_ref()) {
            Some(LoopVerdict::Fixed {
                commit: Some(sha), ..
            }) => sha.get(..12).unwrap_or(sha).to_owned(),
            _ => String::new(),
        };
        let reason = format!("repeats {of}, which was fixed in {commit}");
        self.handoff
            .with_ledger(|ledger| ledger.give(finding, LoopVerdict::Unsettled { reason }));
        self.handoff.record_verdicts(&[finding]).await;
        Some(LoopStop::RepeatingFinding { finding, of })
    }

    /// The findings the reviewer reported in `round`, for the executor, or why
    /// the loop stops. Only a finished round can converge: prose, an empty
    /// reply or a cut-off answer after the nudges is no review.
    fn reported(&self, round: u32) -> Result<Vec<String>, LoopStop> {
        if !self.handoff.finished() {
            return Err(LoopStop::SessionFailed(format!(
                "the reviewer did not finish round {round}"
            )));
        }
        let ledger = self.handoff.ledger();
        let findings: Vec<String> = ledger.reported_in(round).map(finding_text).collect();
        if findings.is_empty() {
            return Err(LoopStop::Converged);
        }
        Ok(findings)
    }

    /// Why the loop stops when a session's round did not end its turn:
    /// a cancel, a time or turn limit, a model error or refusal.
    fn broke_off(&self, who: &str, round: &RoundOutcome) -> Option<LoopStop> {
        let which = self.round;
        let why = match &round.stop {
            StopCause::EndTurn => return None,
            StopCause::Cancelled => return Some(LoopStop::Cancelled),
            StopCause::Timeout => {
                return Some(LoopStop::Timeout(format!(
                    "the {who} reached its time limit in round {which}"
                )));
            }
            StopCause::MaxTurns => "reached its turn limit".to_owned(),
            StopCause::ModelError(error) => format!("failed: {error}"),
            StopCause::Refused(why) => format!("declined ({why})"),
            StopCause::Stuck { tool, .. } => format!("kept repeating {tool}"),
        };
        Some(LoopStop::SessionFailed(format!(
            "the {who} {why} in round {which}"
        )))
    }

    /// Pushes what the executor changed as one commit, once the changeset
    /// passes Henk's rules; the workspace then counts from the new head.
    /// Henk runs no checks of his own: the executor runs the repository's
    /// checks, and the pull request's CI is the final word (#396).
    async fn push_round(
        &mut self,
        facts: &mut PullFacts,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Pushed> {
        let app = self.on.app;
        let Some(address) = app.settings.roles.executor.as_ref() else {
            return Err(anyhow!("the review loop needs [roles.executor]"));
        };
        let exported = self
            .workspace
            .export()
            .await
            .context("listing the changes")?;
        if exported.is_empty() {
            return Ok(Pushed::Nothing);
        }
        let changes = match checked_changeset(exported, address.max_changed_files) {
            Ok(changes) => changes,
            Err(why) => {
                return Ok(Pushed::Stopped(LoopStop::PushRefused(format!(
                    "the executor's changes in round {} cannot be pushed: {why}",
                    self.round
                ))));
            }
        };
        stop_if_cancelled(cancel)?;
        let pusher = Pusher {
            app,
            writer: Arc::clone(&self.writer),
            repo: &self.on.target.repo,
            run: self.on.run,
            requester: self.on.requester,
        };
        let name = format!("henk-loop-{}-push-{}", self.on.run, self.round);
        // Asked again for each push: a loop can outlast the token it
        // started with.
        let credential = self
            .writer
            .git_credential()
            .await
            .context("getting a credential for git")?;
        let checkout = pusher.checkout_pull(&name, facts, credential).await?;
        let notes = [format!("review loop round {}", self.round)];
        let to = PushTo::Pull {
            target: self.on.target,
            facts,
        };
        let pushing = pusher.commit_and_push(&checkout, &to, FIX_SUBJECT, &notes, &changes, cancel);
        let sha = match pushing.await {
            Ok(sha) => sha,
            Err(error) if is_cancelled(&error) => return Err(error),
            Err(error) if error.is::<MergedMeanwhile>() => {
                return Ok(Pushed::Stopped(LoopStop::Merged));
            }
            Err(error) => {
                return Ok(Pushed::Stopped(LoopStop::PushRefused(format!(
                    "the push of round {} was refused: {error:#}",
                    self.round
                ))));
            }
        };
        // On the branch now: whatever fails next, the new head gets the
        // review's check.
        let new_head = CommitSha::parse(&sha).context("the pushed commit")?;
        if let Ok(mut head) = self.on.pushed_head.lock() {
            *head = Some(new_head.clone());
        }
        self.handoff.with_ledger(|ledger| ledger.pushed(&sha));
        self.pushed.push(sha);
        facts.head = new_head;
        let short = facts.head.short().to_owned();
        let patch = head_patch(&checkout).await;
        self.workspace
            .baseline()
            .await
            .context("counting from the pushed commit")?;
        if let Ok(mut state) = self.fixing.state.lock() {
            state.written.clear();
        }
        stages::mark(
            &*app.store,
            self.on.run,
            Stage::Push,
            StageState::Done,
            format!("pushed {short} in round {}", self.round),
        )
        .await;
        Ok(Pushed::Commit { sha: short, patch })
    }
}

/// The executor's system prompt and tools: editing and the checks in
/// `fixing`'s workspace, and its side of the handoff, cut to what its role
/// grants.
fn executor_setup(
    app: &App,
    address: &crate::config::ExecutorConfig,
    fixing: &Arc<EditContext>,
    handoff: &Arc<Handoff>,
    reference: &str,
    repo: &str,
) -> (String, ToolSet) {
    let mut system = format!(
        "{}\n\n{}",
        prompts::PERSONA,
        prompts::render(
            prompts::LOOP_EXECUTOR,
            &[
                ("ref", reference),
                ("repo", repo),
                ("max_files", &address.max_changed_files.to_string()),
            ],
        )
    );
    if fixing.bash {
        system = format!("{system}\n\n{}", prompts::EXECUTOR_BASH);
    }
    let mut tools = edit_tools(fixing);
    tools.add(ListFindings(Arc::clone(handoff)));
    tools.add(GiveVerdict(Arc::clone(handoff)));
    let tools = equip_and_grant(
        Role::Executor,
        &mut system,
        tools,
        app.settings.skills.select(&address.skills),
        &address.tools(),
    );
    (system, tools)
}

/// The reviewer's tools: the diff, the workspace to read and, when the
/// backend allows it (`bash` is its command limit), to run commands in,
/// and the handoff. What it changes in the workspace is never exported.
fn reviewer_tools(
    workspace: &Arc<dyn Workspace>,
    diff: &Arc<ReviewDiff>,
    handoff: &Arc<Handoff>,
    bash: Option<Duration>,
) -> ToolSet {
    let reviewing = never_exported(Arc::clone(workspace));
    let files = Arc::new(DiffFiles::new(Arc::clone(diff)));
    let mut tools = ToolSet::new();
    tools.add(ListChangedFiles(Arc::clone(&files)));
    tools.add(GetFileDiff(files));
    crate::code_tools::add(&mut tools, &reviewing);
    if let Some(limit) = bash {
        crate::code_tools::add_bash(
            &mut tools,
            &reviewing,
            limit,
            crate::code_tools::BashUse::Review,
        );
    }
    tools.add(ReportFinding(Arc::clone(handoff)));
    tools.add(FinishRound(Arc::clone(handoff)));
    tools
}

/// The loop's two roles (#322): the reviewer and the executor.
fn loop_roles(
    settings: &crate::config::Settings,
) -> anyhow::Result<(&crate::config::RoleConfig, &crate::config::ExecutorConfig)> {
    let roles = &settings.roles;
    match (roles.reviewer.as_ref(), roles.executor.as_ref()) {
        (Some(reviewer), Some(executor)) => Ok((reviewer, executor)),
        _ => Err(anyhow!(
            "the review loop needs [roles.reviewer] and [roles.executor]"
        )),
    }
}

/// One session's limits per round: its role's turns and time, the
/// review's conversation budget, and its prompt cache's lifetime: each
/// waits for the other's round, so past 5 minutes it keeps the cache for
/// the hour (#330).
fn round_limits(
    settings: &crate::config::Settings,
    max_turns: u32,
    timeout_secs: u64,
    partner_secs: u64,
) -> AgentConfig {
    AgentConfig {
        max_turns,
        cache_ttl: cache_ttl_for(partner_secs),
        timeout: Duration::from_secs(timeout_secs),
        max_conversation_chars: settings.review.max_conversation_chars,
        keep_recent_turns: settings.review.keep_recent_turns,
        max_repeated_calls: settings.agent.max_repeated_calls,
        record_argument_bytes: settings.agent.record_argument_bytes,
        ..AgentConfig::default()
    }
}

/// What one round's push did.
enum Pushed {
    /// The executor changed nothing.
    Nothing,
    /// One commit, with its patch for the reviewer.
    Commit { sha: String, patch: String },
    /// The loop cannot go on, and why.
    Stopped(LoopStop),
}

/// The reviewer's first message: the pull request and its changed files.
fn opening(target: &ReviewTarget, commit: &CommitSha, diff: &ReviewDiff) -> String {
    format!(
        "Review pull request #{} at commit {}. The changed files:\n{}\nRead their diffs with get_file_diff, several paths per call, and the code around them in your workspace.",
        target.number,
        commit.short(),
        diff.render_list().trim_end()
    )
}

/// The patch of the commit at the checkout's head, cut to
/// [`MAX_PATCH_CHARS`]; empty when git cannot say.
async fn head_patch(checkout: &Checkout) -> String {
    let output = git_command(
        checkout.path(),
        &["show", "--format=", "--patch", "HEAD"],
        None,
    )
    .output()
    .await;
    let Ok(output) = output else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut patch: String = text.chars().take(MAX_PATCH_CHARS).collect();
    if text.chars().count() > MAX_PATCH_CHARS {
        let _ = write!(patch, "\n[cut at {MAX_PATCH_CHARS} characters]");
    }
    patch
}
