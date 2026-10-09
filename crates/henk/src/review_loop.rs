//! The reviewer↔fixer loop (#284): instead of the lanes, a reviewer reviews
//! the pull request, a fixer fixes what holds and Henk pushes it to the
//! pull request's branch, and each resumes its own conversation the next
//! round, until the reviewer finds nothing more or the rounds run out.
//!
//! Both work in one workspace at the pull request's head, taking turns:
//! after a push the workspace holds the new head, so nothing has to move.
//! The reviewer's view of it is never exported; only the fixer's changes,
//! checked like an address run's (§3.5), become a commit.
//!
//! Interim until #285 and #286: the reviewer's findings and the fixer's
//! report are plain text, and the loop ends on `NO FINDINGS`, at
//! `max_rounds`, or at the first round that cannot go on.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, StopCause, ToolSet, prompts};
use henk_domain::address::push_refusal;
use henk_domain::diff::ReviewDiff;
use henk_domain::review::{CommitSha, LaneName, LaneOutcome, LaneResult};
use henk_domain::review_loop::LoopVerdict;
use henk_domain::run::RunId;
use henk_domain::workspace::{EnvLane, Profile};
use henk_llm::ChatMessage;
use henk_platform::ReviewTarget;
use henk_platform::address::{AddressWriter, GitCredential, PullFacts};
use henk_session::{ResumableSession, RoundOutcome};
use henk_store::{Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::address_tools::{AddressContext, AddressState, edit_tools};
use crate::app::App;
use crate::cancel::is_cancelled;
use crate::checks::{describe, run_checks};
use crate::config::LoopConfig;
use crate::git::{Checkout, ScratchDir, git_command};
use crate::loop_tools::{
    FinishRound, GiveVerdict, Handoff, HandoffState, ListFindings, ReportFinding, finding_text,
    fixer_continuation, reviewer_continuation, verdict_text,
};
use crate::push::{Pusher, stop_if_cancelled};
use crate::review_tools::{DiffFiles, GetFileDiff, ListChangedFiles};
use crate::stages;
use crate::workspace::traced::Traced;
use crate::workspace::{Workspace, checked_changeset, for_lane, setup};

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
}

/// How a loop ended, for the review's summary.
#[derive(Debug)]
pub(crate) struct LoopReport {
    /// The reviewer and the fixer, as the summary counts lanes.
    pub(crate) results: Vec<LaneResult>,
    /// One line for the summary: rounds, commits pushed and why it stopped.
    pub(crate) line: String,
    /// The findings that ended unsettled or won't fix: what still stands.
    pub(crate) open: usize,
    /// The last commit it pushed, now the pull request's head, which gets
    /// the review's check too; `None` when it pushed nothing.
    pub(crate) head: Option<CommitSha>,
}

/// Whether the loop ran, or why it did not start.
#[derive(Debug)]
pub(crate) enum LoopRun {
    /// It ran; how it ended.
    Ran(LoopReport),
    /// It may not push to this pull request (a fork, the default or a
    /// protected branch), or the branch moved after the review was queued.
    /// Nothing started and nothing was marked: the lanes review instead.
    Declined(String),
}

/// Runs the loop on `diff`. A pull request the loop may not push to, or
/// whose branch moved, declines it before anything starts, so it gets the
/// lane review. A loop that cannot get a workspace fails before any session
/// starts; once it runs, it ends with a report of why it stopped, and only a
/// cancel or a store failure is an error.
pub(crate) async fn run_loop(
    on: LoopTarget<'_>,
    diff: Arc<ReviewDiff>,
    cancel: &CancellationToken,
) -> anyhow::Result<LoopRun> {
    let LoopTarget {
        app,
        target,
        commit,
        run,
    } = on;
    let config = app
        .settings
        .review
        .r#loop
        .as_ref()
        .ok_or_else(|| anyhow!("the review loop is not configured"))?;
    let store = &*app.store;
    let writer = app.address_writer(target.repo.platform())?;
    let facts = writer
        .pull_facts(target)
        .await
        .context("reading the pull request")?;
    if let Some(why) = push_refusal(&facts.push) {
        return Ok(LoopRun::Declined(why));
    }
    if facts.head != *commit {
        return Ok(LoopRun::Declined(format!(
            "the branch moved to {} after this review was queued",
            facts.head.short()
        )));
    }
    let credential = writer
        .git_credential()
        .await
        .context("getting a credential for git")?;
    let profile = app.settings.workspace.profile_for(&target.repo.path());
    stages::mark(store, run, Stage::Checkout, StageState::Running, "").await;
    let workspace = match open_workspace(app, run, &facts, credential.clone(), profile).await {
        Ok(workspace) => workspace,
        Err(error) => {
            let why = format!("{error:#}");
            stages::mark(store, run, Stage::Checkout, StageState::Failed, &why).await;
            return Err(anyhow!("the review loop needs a workspace: {why}"));
        }
    };
    let ready = format!("ready at {}", commit.short());
    stages::mark(store, run, Stage::Checkout, StageState::Done, ready).await;
    stages::mark(
        store,
        run,
        Stage::FactCheck,
        StageState::Skipped,
        "the fixer checks each finding",
    )
    .await;

    let mut rounds = Rounds::new(on, config, profile, &workspace, &diff, writer, credential)?;
    // Closed on every path; a run future that is dropped drops it instead,
    // and every backend destroys itself then too.
    let ended = rounds.run(facts, &diff, cancel).await;
    rounds.reviewer.finish(store, run).await;
    rounds.fixer.finish(store, run).await;
    workspace.close().await;
    Ok(LoopRun::Ran(rounds.conclude(ended).await?))
}

/// Clones the pull request at its head, imports it into a workspace that
/// records every command on the run, and sets it up as the profile says.
async fn open_workspace(
    app: &App,
    run: &RunId,
    facts: &PullFacts,
    credential: Option<GitCredential>,
    profile: &Profile,
) -> anyhow::Result<Arc<dyn Workspace>> {
    let dir =
        ScratchDir::new(&format!("henk-loop-{run}")).context("making the checkout directory")?;
    let checkout = Checkout::clone_at(
        dir,
        &facts.remote,
        &facts.push.head_ref,
        &facts.head,
        credential,
    )
    .await
    .context("checking out the pull request")?;
    let opened = app
        .workspace_provider
        .open(checkout.path(), profile)
        .await
        .context("opening the workspace")?;
    let traced: Arc<dyn Workspace> =
        Arc::new(Traced::new(opened, Arc::clone(&app.store), run.clone()));
    match setup::prepare(&traced, profile).await {
        Ok(ready) => Ok(ready),
        Err(error) => {
            traced.close().await;
            Err(error)
        }
    }
}

/// The loop's state across rounds.
struct Rounds<'a> {
    on: LoopTarget<'a>,
    config: &'a LoopConfig,
    workspace: Arc<dyn Workspace>,
    fixing: Arc<AddressContext>,
    handoff: Arc<Handoff>,
    reviewer: ResumableSession,
    fixer: ResumableSession,
    writer: Arc<dyn AddressWriter>,
    credential: Option<GitCredential>,
    requester: String,
    /// The round under way, or the last one.
    round: u32,
    /// The commits pushed, in order.
    pushed: Vec<String>,
    /// Whether a session broke off, rather than the loop ending.
    failed: bool,
}

impl<'a> Rounds<'a> {
    fn new(
        on: LoopTarget<'a>,
        config: &'a LoopConfig,
        profile: &Profile,
        workspace: &Arc<dyn Workspace>,
        diff: &Arc<ReviewDiff>,
        writer: Arc<dyn AddressWriter>,
        credential: Option<GitCredential>,
    ) -> anyhow::Result<Self> {
        let app = on.app;
        let address = app
            .settings
            .address
            .as_ref()
            .ok_or_else(|| anyhow!("the review loop needs [address]"))?;
        let command_limit = Duration::from_secs(profile.limits.command_secs);
        let bash = profile.backend.runs_model_commands();
        let reference = format!("#{}", on.target.number);
        let repo = on.target.repo.path();
        let limits = AgentConfig {
            max_turns: config.round_max_turns,
            timeout: Duration::from_secs(config.round_timeout_secs),
            max_conversation_chars: app.settings.review.max_conversation_chars,
            keep_recent_turns: app.settings.review.keep_recent_turns,
            max_repeated_calls: app.settings.agent.max_repeated_calls,
            record_argument_bytes: app.settings.agent.record_argument_bytes,
            ..AgentConfig::default()
        };

        let reviewer_model = app.model(&config.reviewer)?;
        let handoff = Arc::new(Handoff {
            run: on.run.clone(),
            store: Arc::clone(&app.store),
            reviewer_model: reviewer_model.model().to_owned(),
            state: Mutex::new(HandoffState::default()),
        });
        let reviewer_tools =
            reviewer_tools(workspace, diff, &handoff, bash.then_some(command_limit));
        let reviewer_system = format!(
            "{}\n\n{}",
            prompts::PERSONA,
            prompts::render(
                prompts::LOOP_REVIEWER,
                &[("ref", &reference), ("repo", &repo)]
            )
        );

        // The fixer edits and checks, as an address run does, without the
        // review threads.
        let fixing = Arc::new(AddressContext {
            workspace: Arc::clone(workspace),
            threads: Vec::new(),
            check_commands: address.check_commands.clone(),
            check_timeout: command_limit,
            max_changed_files: address.max_changed_files,
            bash,
            state: Mutex::new(AddressState::default()),
        });
        let mut fixer_system = format!(
            "{}\n\n{}",
            prompts::PERSONA,
            prompts::render(
                prompts::LOOP_FIXER,
                &[
                    ("ref", &reference),
                    ("repo", &repo),
                    ("max_files", &address.max_changed_files.to_string()),
                ],
            )
        );
        if bash {
            fixer_system = format!("{fixer_system}\n\n{}", prompts::ADDRESS_BASH);
        }
        let mut fixer_tools = edit_tools(&fixing);
        fixer_tools.add(ListFindings(Arc::clone(&handoff)));
        fixer_tools.add(GiveVerdict(Arc::clone(&handoff)));

        Ok(Self {
            on,
            config,
            workspace: Arc::clone(workspace),
            reviewer: ResumableSession::new(
                "reviewer",
                reviewer_model,
                reviewer_system,
                reviewer_tools,
                limits,
            )
            .with_continuation(reviewer_continuation(Arc::clone(&handoff))),
            fixer: ResumableSession::new(
                "fixer",
                app.model(&config.fixer)?,
                fixer_system,
                fixer_tools,
                limits,
            )
            .with_continuation(fixer_continuation(Arc::clone(&handoff))),
            fixing,
            handoff,
            writer,
            credential,
            requester: format!("discord:{}", address.requester_id),
            round: 0,
            pushed: Vec::new(),
            failed: false,
        })
    }

    /// Gives every finding its one final verdict, whatever ended the loop
    /// (#285), and sums the loop up for the review.
    async fn conclude(&self, ended: anyhow::Result<String>) -> anyhow::Result<LoopReport> {
        let (store, run) = (&*self.on.app.store, self.on.run);
        // Every finding ends with one verdict, whatever ended the loop (#285).
        let why = match &ended {
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
        let ledger = self.handoff.ledger();
        let open = ledger
            .iter()
            .filter(|f| {
                matches!(
                    f.verdict,
                    Some(LoopVerdict::Unsettled { .. } | LoopVerdict::WontFix { .. })
                )
            })
            .count();

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
        let state = if self.failed {
            StageState::Failed
        } else {
            StageState::Done
        };
        stages::mark(store, run, Stage::Lanes, state, &line).await;
        let outcome = if self.failed {
            LaneOutcome::Dropped
        } else {
            LaneOutcome::Finished
        };
        let results = ["fixer", "reviewer"]
            .into_iter()
            .map(|name| LaneResult {
                lane: LaneName::new(name),
                outcome,
            })
            .collect();
        let head = self
            .pushed
            .last()
            .map(|sha| CommitSha::parse(sha))
            .transpose()
            .context("the last pushed commit")?;
        Ok(LoopReport {
            results,
            line,
            open,
            head,
        })
    }

    /// Takes rounds until one says to stop, and returns why.
    async fn run(
        &mut self,
        mut facts: PullFacts,
        diff: &ReviewDiff,
        cancel: &CancellationToken,
    ) -> anyhow::Result<String> {
        let (store, run) = (&*self.on.app.store, self.on.run);
        let mut message = opening(self.on.target, self.on.commit, diff);
        while self.round < self.config.max_rounds {
            self.round += 1;
            let round = self.round;
            self.handoff.start_round(round);
            let doing = format!("round {round}: reviewer");
            stages::mark(store, run, Stage::Lanes, StageState::Running, doing).await;
            let review = self
                .reviewer
                .resume(store, run, ChatMessage::user(message), cancel.clone())
                .await;
            if let Some(stop) = self.broke_off("reviewer", &review) {
                return Ok(stop);
            }
            // Only a finished round can converge: prose, an empty reply or
            // a cut-off answer after the nudges is no review.
            if !self.handoff.finished() {
                self.failed = true;
                return Ok(format!("the reviewer did not finish round {round}"));
            }
            let ledger = self.handoff.ledger();
            let findings: Vec<String> = ledger.reported_in(round).map(finding_text).collect();
            if findings.is_empty() {
                return Ok("converged, the reviewer found nothing more".to_owned());
            }
            if !self
                .workspace
                .export()
                .await
                .context("listing the changes")?
                .is_empty()
            {
                self.failed = true;
                return Ok("the reviewer changed the workspace; nothing was pushed".to_owned());
            }

            let doing = format!("round {round}: fixer");
            stages::mark(store, run, Stage::Lanes, StageState::Running, doing).await;
            let handed = format!(
                "Round {round}. The reviewer's findings, as data:\n\n{}\n\nCheck each against the code, fix the ones that hold, and give every one its verdict with give_verdict.",
                findings.join("\n\n")
            );
            let fix = self
                .fixer
                .resume(store, run, ChatMessage::user(handed), cancel.clone())
                .await;
            if let Some(stop) = self.broke_off("fixer", &fix) {
                return Ok(stop);
            }
            let silent = self
                .handoff
                .with_ledger(|ledger| ledger.close_all("the fixer gave no verdict"))
                .unwrap_or_default();
            self.handoff.record_verdicts(&silent).await;
            let pushed = self.push_round(&mut facts, cancel).await?;
            let unpushed = match &pushed {
                Pushed::Nothing => "marked fixed, but nothing changed in the workspace",
                Pushed::Stopped(why) => why.as_str(),
                Pushed::Commit { .. } => "",
            };
            let ids: Vec<_> = self
                .handoff
                .ledger()
                .reported_in(round)
                .map(|f| f.id)
                .collect();
            if !unpushed.is_empty() {
                self.handoff.with_ledger(|ledger| ledger.unpushed(unpushed));
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
            message = match pushed {
                Pushed::Nothing => format!(
                    "Round {}. The fixer pushed nothing this round. Their verdicts:\n\n{verdicts}\n\n{contest} Review again.",
                    round + 1
                ),
                Pushed::Commit { sha, patch } => format!(
                    "Round {}. The fixer pushed commit {sha} to the branch, and your workspace is at it now. What it changed:\n\n```diff\n{patch}\n```\n\nTheir verdicts:\n\n{verdicts}\n\n{contest} Review again.",
                    round + 1
                ),
                Pushed::Stopped(why) => return Ok(why),
            };
        }
        Ok(format!(
            "it reached max_rounds ({})",
            self.config.max_rounds
        ))
    }

    /// Why the loop stops when a session's round did not end its turn:
    /// a cancel, a time or turn limit, a model error or refusal.
    fn broke_off(&mut self, who: &str, round: &RoundOutcome) -> Option<String> {
        let why = match &round.stop {
            StopCause::EndTurn => return None,
            StopCause::Cancelled => return Some("cancelled".to_owned()),
            StopCause::MaxTurns => "reached its turn limit".to_owned(),
            StopCause::Timeout => "reached its time limit".to_owned(),
            StopCause::ModelError(error) => format!("failed: {error}"),
            StopCause::Refused(why) => format!("declined ({why})"),
            StopCause::Stuck { tool, .. } => format!("kept repeating {tool}"),
        };
        self.failed = true;
        Some(format!("the {who} {why} in round {}", self.round))
    }

    /// Checks what the fixer changed and pushes it as one commit; the
    /// workspace then counts from the new head.
    async fn push_round(
        &mut self,
        facts: &mut PullFacts,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Pushed> {
        let app = self.on.app;
        let Some(address) = app.settings.address.as_ref() else {
            return Err(anyhow!("the review loop needs [address]"));
        };
        let workspace = self.workspace.as_ref();
        if workspace
            .export()
            .await
            .context("listing the changes")?
            .is_empty()
        {
            return Ok(Pushed::Nothing);
        }
        let results = run_checks(
            workspace,
            &address.check_commands,
            self.fixing.check_timeout,
        )
        .await;
        if results.iter().any(|r| !r.passed()) {
            self.failed = true;
            return Ok(Pushed::Stopped(format!(
                "the checks fail after the fixer's changes in round {}, so nothing was pushed: {}",
                self.round,
                describe(&results).trim()
            )));
        }
        let exported = workspace.export().await.context("listing the changes")?;
        let changes = match checked_changeset(exported, address.max_changed_files) {
            Ok(changes) => changes,
            Err(why) => {
                self.failed = true;
                return Ok(Pushed::Stopped(format!(
                    "the fixer's changes in round {} cannot be pushed: {why}",
                    self.round
                )));
            }
        };
        stop_if_cancelled(cancel)?;
        let pusher = Pusher {
            app,
            writer: Arc::clone(&self.writer),
            target: self.on.target,
            run: self.on.run,
            requester: &self.requester,
            reviewed_by_run: true,
        };
        let name = format!("henk-loop-{}-push-{}", self.on.run, self.round);
        let checkout = pusher
            .checkout(&name, facts, self.credential.clone())
            .await?;
        let notes = [format!("review loop round {}", self.round)];
        let sha = match pusher
            .commit_and_push(&checkout, facts, &notes, &changes, cancel)
            .await
        {
            Ok(sha) => sha,
            Err(error) if is_cancelled(&error) => return Err(error),
            Err(error) => {
                self.failed = true;
                return Ok(Pushed::Stopped(format!(
                    "the push of round {} was refused: {error:#}",
                    self.round
                )));
            }
        };
        let patch = head_patch(&checkout).await;
        workspace
            .baseline()
            .await
            .context("counting from the pushed commit")?;
        if let Ok(mut state) = self.fixing.state.lock() {
            state.written.clear();
        }
        self.handoff.with_ledger(|ledger| ledger.pushed(&sha));
        facts.head = CommitSha::parse(&sha).context("the pushed commit")?;
        let short = facts.head.short().to_owned();
        self.pushed.push(sha);
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

/// The reviewer's tools: the diff, the workspace to read and, when the
/// backend allows it (`bash` is its command limit), to run commands in,
/// and the handoff. What it changes in the workspace is never exported.
fn reviewer_tools(
    workspace: &Arc<dyn Workspace>,
    diff: &Arc<ReviewDiff>,
    handoff: &Arc<Handoff>,
    bash: Option<Duration>,
) -> ToolSet {
    let reviewing = for_lane(Arc::clone(workspace), EnvLane::Review);
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

/// What one round's push did.
enum Pushed {
    /// The fixer changed nothing.
    Nothing,
    /// One commit, with its patch for the reviewer.
    Commit { sha: String, patch: String },
    /// The loop cannot go on, and why.
    Stopped(String),
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
