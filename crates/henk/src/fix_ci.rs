//! The fix-CI run (§3.7, #583): Henk reads the failing checks of a pull
//! request's head, has the executor fix the code in a workspace at that
//! head, and pushes the fix to the branch as a fast-forward, with the guards
//! of every push to a pull request. Or the executor says the failure is not
//! about the code (a lost runner, a service that was down), and nothing is
//! pushed.
//!
//! Every fix-CI run ends with one comment on the pull request: what failed
//! and what changed, that nothing fails, or why nothing was pushed. A pushed
//! fix is then reviewed like any other head, in a run of its own that the
//! coordinator asks for (or the caller, when it waited for this run), since
//! the platform's event about Henk's own push is dropped as his own (§8.1).

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, prompts};
use henk_domain::commit::{PlannedCommit, bullet_body, fix_ci_subject, planned_commits};
use henk_domain::identity::RequesterId;
use henk_domain::push::push_refusal;
use henk_domain::review::CommitSha;
use henk_domain::role::Role;
use henk_domain::run::{RunId, RunKind};
use henk_domain::workspace::Change;
use henk_llm::ChatMessage;
use henk_platform::push::{PullFacts, PushWriter};
use henk_platform::{CiJob, ReviewTarget};
use henk_session::{SessionSpec, model_id, run_session};
use henk_store::{NewRun, RunStatus, Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::app::App;
use crate::cancel::Cancelled;
use crate::ci_tools::{CiScope, clean};
use crate::commit_tools::{RECENT_COMMITS, recent_commits_text};
use crate::config::{EditFormat, ExecutorConfig, ModelFileConfig};
use crate::edit_tools::{EditContext, EditState, edit_tools};
use crate::execute::session_result;
use crate::fix_ci_tools::{Cause, FixProposal, FixProposed, ProposeFix, fix_continuation};
use crate::ids::new_run_id;
use crate::issue_runs::{OnRun, PullThread};
use crate::liveness::KeepAlive;
use crate::push::{MergedMeanwhile, PushTo, Pusher, stop_if_cancelled};
use crate::review::ReviewRequest;
use crate::roles::equip_and_grant;
use crate::stages;
use crate::workspace::{Workspace, checked_changeset, open_prepared};

/// How long one fix-CI run may take, reading the logs, the session and the
/// push included.
const RUN_TIMEOUT: Duration = Duration::from_mins(30);

/// A request to fix the failing checks of one pull request.
#[derive(Debug, Clone)]
pub struct FixCiRequest {
    /// The pull request.
    pub target: ReviewTarget,
    /// What started it.
    pub trigger: String,
    /// The run id to use, when the caller already announced one.
    pub run: Option<RunId>,
    /// Who asked, for the run record and the commits: a stable id such as
    /// `github:<id>`, a commenter's login, or nobody.
    pub requester: Option<String>,
}

/// How a fix-CI run ended well. Only [`FixOutcome::Pushed`] changed the
/// pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixOutcome {
    /// A fix was pushed; the head is now `head`.
    Pushed {
        /// The failing checks it was for.
        checks: Vec<String>,
        /// What the executor says it did.
        summary: String,
        /// The pushed head.
        head: CommitSha,
        /// Commits pushed.
        commits: usize,
    },
    /// The failure is not about the code; nothing was pushed.
    NotCode {
        /// The checks.
        checks: Vec<String>,
        /// Why not, in the executor's words or Henk's.
        reason: String,
    },
    /// No fix was found; nothing was pushed.
    NoFix {
        /// The checks.
        checks: Vec<String>,
        /// Why.
        why: String,
    },
    /// Nothing is failing to fix right now: the checks are still running,
    /// none fails, or the pull request was merged meanwhile.
    Idle(String),
}

impl FixOutcome {
    /// What Henk says on the pull request.
    #[must_use]
    pub fn said(&self) -> String {
        match self {
            Self::Pushed {
                checks,
                summary,
                head,
                commits,
            } => format!(
                "I pushed a fix for the failing checks ({}): {summary} {commits} commit(s), now at {}. A review of the fix follows.",
                listed(checks),
                head.short()
            ),
            Self::NotCode { checks, reason } => format!(
                "The failing checks ({}) do not look like a problem in the code: {reason} I pushed nothing.",
                listed(checks)
            ),
            Self::NoFix { checks, why } => format!(
                "I could not fix the failing checks ({}): {why}. I pushed nothing.",
                listed(checks)
            ),
            Self::Idle(why) => format!("There is nothing to fix right now: {why}."),
        }
    }
}

/// Check names for a sentence: each on one line, in backticks.
fn listed(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("`{}`", clean(name)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How a fix-CI run ended well.
#[derive(Debug)]
pub struct FixCiReport {
    /// The run.
    pub run: RunId,
    /// The pull request.
    pub target: ReviewTarget,
    /// What came of it.
    pub outcome: FixOutcome,
}

impl FixCiReport {
    /// The head Henk pushed, when he pushed one.
    #[must_use]
    pub fn pushed(&self) -> Option<&CommitSha> {
        match &self.outcome {
            FixOutcome::Pushed { head, .. } => Some(head),
            _ => None,
        }
    }

    /// The review of the fixed head, when there is one: a run of its own at
    /// the commit Henk pushed. `requester` is who asked for the fix.
    #[must_use]
    pub fn review(&self, requester: Option<String>) -> Option<ReviewRequest> {
        let commit = self.pushed()?.clone();
        Some(ReviewRequest {
            target: self.target.clone(),
            commit: Some(commit),
            trigger: format!("fixed by run {}", self.run),
            requester,
            acknowledge: None,
            run: None,
            submitted_at: None,
            queued_check: None,
        })
    }
}

/// Fixes the failing checks of one pull request, end to end. Every fix-CI
/// run ends with one comment on the pull request.
#[instrument(skip_all, fields(repo = %request.target.repo.path(), pull = request.target.number))]
pub async fn run_fix_ci(
    app: &App,
    request: FixCiRequest,
    cancel: CancellationToken,
) -> anyhow::Result<FixCiReport> {
    let executor = app
        .settings
        .roles
        .executor
        .as_ref()
        .ok_or_else(|| anyhow!("fixing checks needs [roles.executor]"))?;
    let target = &request.target;
    if !app.settings.allowlist.allows(&target.repo) {
        return Err(anyhow!("{} is not on the allowlist", target.repo));
    }
    let writer = app.writer(&target.repo)?;

    let run = request.run.clone().unwrap_or_else(new_run_id);
    let link = app.settings.run_link(&run);
    app.store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::FixCi,
            platform: target.repo.platform(),
            repo: target.repo.path(),
            target: target.number,
            commit: None,
            requester: request.requester.clone(),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await?;
    info!(run = %run, "fix of failing checks started");
    let _alive = KeepAlive::start(Arc::clone(&app.store), &app.live_runs, run.clone());
    let model = app.model(&executor.model)?;
    let model_name = model_id(model.as_ref());
    let work = Work {
        app,
        executor,
        request: &request,
        run: &run,
        model,
        deadline: tokio::time::Instant::now() + RUN_TIMEOUT,
    };
    let result = work.run(&cancel).await;
    let on_pull = OnRun {
        app,
        thread: PullThread {
            writer: &writer,
            target,
        },
        run: &run,
        link: &link,
        model: &model_name,
        what: "fix of failing checks",
    };
    match result {
        Ok(outcome) => {
            let said = outcome.said();
            on_pull.post(&said).await;
            stages::finished(&*app.store, &run, &said).await;
            app.store
                .finish_run(&run, RunStatus::Finished, Some(&said), None)
                .await?;
            info!(run = %run, "fix of failing checks ended");
            Ok(FixCiReport {
                run,
                target: target.clone(),
                outcome,
            })
        }
        Err(failure) => {
            if let Some(ended) = on_pull.ended_early(&failure).await {
                return Err(ended);
            }
            let reason = format!("{failure:#}");
            let comment = format!(
                "I could not fix the failing checks of this pull request: {reason}. Nothing was pushed."
            );
            Err(on_pull.failed(&comment, &reason).await)
        }
    }
}

/// What the first read of the pull request found.
enum Read {
    /// These checks fail on the head.
    Failing(PullFacts, Vec<CiJob>),
    /// There is nothing for the executor to do.
    Settled(FixOutcome),
}

/// One fix-CI run's work, from reading the pull request to the push.
struct Work<'a> {
    app: &'a App,
    executor: &'a ExecutorConfig,
    request: &'a FixCiRequest,
    run: &'a RunId,
    model: Arc<dyn henk_llm::ModelClient>,
    deadline: tokio::time::Instant,
}

impl Work<'_> {
    async fn run(&self, cancel: &CancellationToken) -> anyhow::Result<FixOutcome> {
        let (store, run, target) = (&*self.app.store, self.run, &self.request.target);
        stages::requested(
            store,
            run,
            None,
            &self.request.trigger,
            self.request.requester.as_deref(),
        )
        .await;
        let what = format!("fixing the failing checks of #{}", target.number);
        stages::mark(store, run, Stage::Started, StageState::Done, what).await;
        // Cancelled while it waited for a slot (#580): it ends here.
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        if !self.executor.fixes_ci(&target.repo) {
            return Err(anyhow!(
                "fixing checks is off for {} ([roles.executor.repositories] fix_ci = false)",
                target.repo.path()
            ));
        }
        if let Some(tool) = Role::Executor.missing_fix_ci_handoff(&self.executor.tools()) {
            return Err(anyhow!(
                "[roles.executor] tools leave out {tool}, which fixing checks needs"
            ));
        }

        stages::mark(store, run, Stage::Checkout, StageState::Running, "").await;
        let writer = self.app.push_writer(&target.repo)?;
        let read = tokio::select! {
            read = self.read(&*writer) => read,
            () = cancel.cancelled() => return Err(Cancelled.into()),
        };
        let (facts, failing) = match read {
            Ok(Read::Failing(facts, failing)) => (facts, failing),
            Ok(Read::Settled(outcome)) => {
                stages::mark(
                    store,
                    run,
                    Stage::Checkout,
                    StageState::Done,
                    "nothing to fix",
                )
                .await;
                return Ok(outcome);
            }
            Err(error) => {
                let why = format!("{error:#}");
                stages::mark(store, run, Stage::Checkout, StageState::Failed, &why).await;
                return Err(error);
            }
        };
        let opened = tokio::select! {
            opened = self.open(&writer, &facts) => opened,
            () = cancel.cancelled() => return Err(Cancelled.into()),
        };
        let (workspace, recent) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let why = format!("{error:#}");
                stages::mark(store, run, Stage::Checkout, StageState::Failed, &why).await;
                return Err(error.context("the executor needs a workspace"));
            }
        };
        let ready = format!("{} at {}", facts.push.head_ref, facts.head.short());
        stages::mark(store, run, Stage::Checkout, StageState::Done, ready).await;
        let result = self
            .fix(&writer, &facts, &failing, (&workspace, &recent), cancel)
            .await;
        workspace.close().await;
        result
    }

    /// The pull request as it is now, once Henk may push to it, and what
    /// its checks say.
    async fn read(&self, writer: &dyn PushWriter) -> anyhow::Result<Read> {
        let target = &self.request.target;
        let facts = writer
            .pull_facts(target)
            .await
            .context("reading the pull request")?;
        if facts.push.merged || !facts.push.open {
            return Err(anyhow!("it is no longer open"));
        }
        if let Some(why) = push_refusal(&facts.push) {
            return Err(anyhow!("I may not push to its branch: {why}"));
        }
        let reader = self
            .app
            .ci_reader(&target.repo)
            .context("reading the checks")?;
        let jobs = reader
            .jobs(target, &facts.head)
            .await
            .context("reading the checks")?;
        let failed: Vec<CiJob> = jobs.iter().filter(|job| job.failed()).cloned().collect();
        if !failed.is_empty() {
            return Ok(Read::Failing(facts, failed));
        }
        let head = facts.head.short();
        if jobs.iter().any(|job| !job.finished()) {
            return Ok(Read::Settled(FixOutcome::Idle(format!(
                "no check has failed on {head} yet, and some are still running"
            ))));
        }
        let interrupted: Vec<String> = jobs
            .iter()
            .filter(|job| job.interrupted())
            .map(|job| job.name.clone())
            .collect();
        if interrupted.is_empty() {
            return Ok(Read::Settled(FixOutcome::Idle(format!(
                "no check fails on {head}"
            ))));
        }
        Ok(Read::Settled(FixOutcome::NotCode {
            checks: interrupted,
            reason:
                "they were cancelled or ran out of time, which a change to the code does not fix."
                    .to_owned(),
        }))
    }

    /// A checkout of the branch at the head, a workspace on it, and the
    /// branch's latest commit subjects, which show its commit convention
    /// (#400).
    async fn open(
        &self,
        writer: &Arc<dyn PushWriter>,
        facts: &PullFacts,
    ) -> anyhow::Result<(Arc<dyn Workspace>, Vec<String>)> {
        let target = &self.request.target;
        let pusher = self.pusher(writer);
        let credential = writer
            .git_credential()
            .await
            .context("getting a credential for git")?;
        let checkout = pusher
            .checkout_pull(&format!("henk-fix-ci-{}", self.run), facts, credential)
            .await?;
        let recent = checkout.recent_subjects(RECENT_COMMITS).await;
        let profile = self.app.settings.workspace.profile_for(&target.repo.path());
        let workspace =
            open_prepared(self.app, self.run, &checkout, profile, Some(self.deadline)).await?;
        Ok((workspace, recent))
    }

    fn pusher(&self, writer: &Arc<dyn PushWriter>) -> Pusher<'_> {
        Pusher {
            app: self.app,
            writer: Arc::clone(writer),
            repo: &self.request.target.repo,
            run: self.run,
            requester: None,
        }
    }

    /// The executor's session, then the push of what it proposed.
    async fn fix(
        &self,
        writer: &Arc<dyn PushWriter>,
        facts: &PullFacts,
        failing: &[CiJob],
        (workspace, recent): (&Arc<dyn Workspace>, &[String]),
        cancel: &CancellationToken,
    ) -> anyhow::Result<FixOutcome> {
        let (store, run, target) = (&*self.app.store, self.run, &self.request.target);
        let checks: Vec<String> = failing.iter().map(|job| job.name.clone()).collect();
        let profile = self.app.settings.workspace.profile_for(&target.repo.path());
        let editing = Arc::new(EditContext {
            workspace: Arc::clone(workspace),
            command_timeout: Duration::from_secs(profile.limits.command_secs),
            max_changed_files: self.executor.max_changed_files,
            // Told of bash only when it has it: the backend runs model
            // commands and [roles.executor] tools grant it.
            bash: profile.backend.runs_model_commands()
                && Role::Executor.grants(&self.executor.tools(), "bash"),
            edit_format: self
                .app
                .settings
                .models
                .get(&self.executor.model)
                .map_or(EditFormat::Replace, ModelFileConfig::edit_format),
            state: Mutex::new(EditState::default()),
        });
        let reader = self.app.ci_reader(&target.repo)?;
        let scope = Arc::new(CiScope::new(reader, target.clone(), facts.head.clone()));
        let brief = scope.failure_brief(failing).await;
        let proposed: FixProposed = Arc::default();
        let spec = self.session(&editing, &proposed, &scope, facts, recent, &brief);
        let session = format!("executor, {}", self.model.model());
        stages::mark(store, run, Stage::Session, StageState::Running, session).await;
        let outcome = run_session(store, run, spec, cancel.clone()).await;
        session_result(outcome.stop, RUN_TIMEOUT.as_secs())?;

        let proposal = proposed.lock().ok().and_then(|p| p.clone());
        let Some(proposal) = proposal else {
            stages::mark(store, run, Stage::Session, StageState::Done, "no proposal").await;
            return Ok(FixOutcome::NoFix {
                checks,
                why: "the executor ended without proposing a fix".to_owned(),
            });
        };
        if let Cause::NotCode(reason) = proposal.cause {
            // Whatever its workspace holds is left there: nothing is pushed.
            stages::mark(
                store,
                run,
                Stage::Session,
                StageState::Done,
                "not about the code",
            )
            .await;
            return Ok(FixOutcome::NotCode { checks, reason });
        }
        stages::mark(store, run, Stage::Session, StageState::Done, "proposed").await;

        stages::mark(store, run, Stage::Commit, StageState::Running, "").await;
        let exported = workspace.export().await.context("listing the changes")?;
        if exported.is_empty() {
            stages::mark(store, run, Stage::Commit, StageState::Done, "no changes").await;
            return Ok(FixOutcome::NoFix {
                checks,
                why: format!("the executor changed nothing: {}", proposal.summary),
            });
        }
        let changes = checked_changeset(exported, self.executor.max_changed_files)
            .map_err(|why| anyhow!("the executor's changes cannot be pushed: {why}"))?;
        let to_push = format!("{} file(s) to push", changes.len());
        stages::mark(store, run, Stage::Commit, StageState::Done, to_push).await;
        stop_if_cancelled(cancel)?;
        self.push(writer, facts, &proposal, &changes, checks, cancel)
            .await
    }

    /// Commits the checked changes to the pull request's branch and pushes
    /// them as a fast-forward.
    async fn push(
        &self,
        writer: &Arc<dyn PushWriter>,
        facts: &PullFacts,
        proposal: &FixProposal,
        changes: &[Change],
        checks: Vec<String>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<FixOutcome> {
        let (store, run, target) = (&*self.app.store, self.run, &self.request.target);
        stages::mark(store, run, Stage::Push, StageState::Running, "").await;
        let requester = self
            .request
            .requester
            .as_deref()
            .and_then(RequesterId::parse);
        let pusher = Pusher {
            requester: requester.as_ref(),
            ..self.pusher(writer)
        };
        let credential = writer
            .git_credential()
            .await
            .context("getting a credential for git")?;
        // A checkout nothing ran in: only the checked changeset reaches it.
        let checkout = pusher
            .checkout_pull(&format!("henk-fix-ci-{run}-push"), facts, credential)
            .await?;
        let commits = self.commits(&pusher, proposal, changes).await;
        let to = PushTo::Pull { target, facts };
        let shas = match pusher
            .commit_and_push(&checkout, &to, &commits, changes, cancel)
            .await
        {
            Ok(shas) => shas,
            Err(error) if error.is::<MergedMeanwhile>() => {
                stages::mark(
                    store,
                    run,
                    Stage::Push,
                    StageState::Skipped,
                    "merged meanwhile",
                )
                .await;
                return Ok(FixOutcome::Idle(
                    "the pull request was merged while I worked, so nothing was pushed".to_owned(),
                ));
            }
            Err(error) => {
                let why = format!("{error:#}");
                stages::mark(store, run, Stage::Push, StageState::Failed, &why).await;
                return Err(error);
            }
        };
        let head = shas
            .last()
            .context("no commit was pushed")
            .and_then(|sha| CommitSha::parse(sha).context("the pushed commit"))?;
        stages::mark(store, run, Stage::Push, StageState::Done, head.short()).await;
        Ok(FixOutcome::Pushed {
            checks,
            summary: proposal.summary.clone(),
            head,
            commits: shas.len(),
        })
    }

    /// The commits for `changes`: the executor's, when they hold every
    /// change once and are in shape, or else one `Fix failing checks`
    /// commit, with why on the run's timeline (#400).
    async fn commits(
        &self,
        pusher: &Pusher<'_>,
        proposal: &FixProposal,
        changes: &[Change],
    ) -> Vec<PlannedCommit> {
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        if let Some(proposed) = &proposal.commits {
            match planned_commits(proposed, &paths) {
                Ok(planned) => return planned,
                Err(why) => {
                    pusher
                        .note(&format!(
                            "the executor's commits cannot be made ({why}); its change is pushed as one commit"
                        ))
                        .await;
                }
            }
        }
        vec![PlannedCommit {
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            subject: fix_ci_subject(),
            body: bullet_body(std::slice::from_ref(&proposal.summary)),
        }]
    }

    /// The executor's session: one conversation from the failing checks to
    /// its proposal.
    fn session(
        &self,
        editing: &Arc<EditContext>,
        proposed: &FixProposed,
        scope: &Arc<CiScope>,
        facts: &PullFacts,
        recent: &[String],
        brief: &str,
    ) -> SessionSpec {
        let target = &self.request.target;
        let mut system = format!(
            "{}\n\n{}",
            prompts::PERSONA,
            prompts::render(
                prompts::FIX_CI,
                &[
                    ("ref", &format!("#{}", target.number)),
                    ("repo", &target.repo.path()),
                    ("branch", &facts.push.head_ref),
                    ("max_files", &self.executor.max_changed_files.to_string()),
                    ("edit_tool", editing.edit_format.tool()),
                    ("recent_commits", &recent_commits_text(recent)),
                ],
            )
        );
        if editing.bash {
            system = format!("{system}\n\n{}", prompts::EXECUTOR_BASH);
        }
        let mut tools = edit_tools(editing);
        tools.add(ProposeFix {
            proposed: Arc::clone(proposed),
            workspace: Arc::clone(&editing.workspace),
        });
        for tool in crate::ci_tools::tools(scope) {
            tools.add_arc(tool);
        }
        let tools = equip_and_grant(
            Role::Executor,
            &mut system,
            tools,
            self.app.settings.skills.select(&self.executor.skills),
            &self.executor.tools(),
        );
        let mut opening = format!(
            "Fix the failing checks of pull request #{} at commit {}.\n\n{brief}",
            target.number,
            facts.head.as_str()
        );
        let _ = write!(opening, "\nRead more with the CI tools if you need it.");
        let settings = &self.app.settings;
        SessionSpec {
            name: "executor".to_owned(),
            role: Role::Executor,
            model: Arc::clone(&self.model),
            system,
            opening: vec![ChatMessage::user(opening)],
            tools,
            limits: AgentConfig {
                max_turns: self.executor.max_turns,
                timeout: self
                    .deadline
                    .saturating_duration_since(tokio::time::Instant::now()),
                max_repeated_calls: settings.agent.max_repeated_calls,
                record_argument_bytes: settings.agent.record_argument_bytes,
                ..AgentConfig::default()
            },
            continuation: Some(fix_continuation(Arc::clone(proposed))),
            turn_warning: None,
        }
    }
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

    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::push::PushFacts;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{
        Block, Completion, ModelClient, Role as LlmRole, StopReason, ToolArguments, ToolCall, Usage,
    };
    use henk_platform::push::{
        BehindPull, CommitIdentity, GitCredential, NewPull, OpenedPull, RepoHead,
    };
    use henk_platform::{CiConclusion, CiReader, PlatformError, PlatformWriter};
    use serde_json::json;

    use super::*;
    use crate::ci_tools::tests::{FakeCi, job};
    use crate::config::Config;
    use crate::git::ScratchDir;
    use crate::git::tests::{
        diverged_remote, remote_branch_file, remote_branch_log, remote_feature,
    };
    use crate::listeners::testing::FakeWriter;
    use crate::workspace::fake::FakeProvider;

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
[roles.executor]
model = "m"
requester_id = 3
"#;

    /// A pull request whose branch `feature` is on a bare remote beside its
    /// base `main`. `fork` says its branch is in another repository;
    /// `moves_at` makes the platform report another head from that read on,
    /// as if somebody pushed meanwhile.
    struct PullHub {
        remote: std::path::PathBuf,
        fork: bool,
        moves_at: Option<usize>,
        reads: AtomicUsize,
    }

    fn unused() -> PlatformError {
        PlatformError::Decode("not used by a fix of failing checks".to_owned())
    }

    #[async_trait::async_trait]
    impl PushWriter for PullHub {
        async fn pull_facts(&self, _: &ReviewTarget) -> Result<PullFacts, PlatformError> {
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            let (head, _) = remote_feature(&self.remote).await;
            let head = if self.moves_at.is_some_and(|at| read >= at) {
                "d".repeat(40)
            } else {
                head
            };
            Ok(PullFacts {
                push: PushFacts {
                    open: true,
                    merged: false,
                    head_repo: Some(if self.fork { "someone/r" } else { "o/r" }.to_owned()),
                    base_repo: "o/r".to_owned(),
                    head_ref: "feature".to_owned(),
                    default_branch: "main".to_owned(),
                    head_protected: false,
                },
                head: CommitSha::parse(&head).unwrap(),
                remote: self.remote.to_string_lossy().into_owned(),
                base_ref: "main".to_owned(),
            })
        }
        async fn repo_head(&self, _: &RepoRef) -> Result<RepoHead, PlatformError> {
            Err(unused())
        }
        async fn git_credential(&self) -> Result<Option<GitCredential>, PlatformError> {
            Ok(None)
        }
        async fn commit_identity(&self) -> Result<CommitIdentity, PlatformError> {
            Ok(CommitIdentity {
                name: "meneer-henk[bot]".to_owned(),
                email: "1+meneer-henk[bot]@users.noreply.github.com".to_owned(),
            })
        }
        async fn user_login(&self, id: u64) -> Result<String, PlatformError> {
            Ok(format!("user{id}"))
        }
        fn noreply_host(&self) -> Result<String, PlatformError> {
            Ok("github.com".to_owned())
        }
        async fn open_pulls_behind(
            &self,
            _: &RepoRef,
            _: &str,
            _: &str,
        ) -> Result<Vec<BehindPull>, PlatformError> {
            Err(unused())
        }
        async fn open_pull_request(
            &self,
            _: &RepoRef,
            _: &NewPull,
        ) -> Result<OpenedPull, PlatformError> {
            Err(unused())
        }
    }

    struct Fixture {
        app: App,
        writer: Arc<FakeWriter>,
        remote: ScratchDir,
        model: Arc<ScriptedClient>,
    }

    /// The checks of the head: `test` fails, `lint` passes.
    fn red() -> FakeCi {
        let mut ci = FakeCi::default();
        ci.jobs.insert(
            "*".to_owned(),
            vec![
                job("11", "test", Some(CiConclusion::Failure), true),
                job("12", "lint", Some(CiConclusion::Success), true),
            ],
        );
        ci.logs.insert(
            "11".to_owned(),
            "running 3 tests\nthread 'main' panicked at src/a.rs:1: x was 1, wanted 3\nFAILED\n"
                .to_owned(),
        );
        ci
    }

    async fn fixture(name: &str, script: Vec<Completion>, ci: FakeCi, extra: &str) -> Fixture {
        fixture_on(name, script, ci, extra, (false, None)).await
    }

    async fn fixture_on(
        name: &str,
        script: Vec<Completion>,
        ci: FakeCi,
        extra: &str,
        (fork, moves_at): (bool, Option<usize>),
    ) -> Fixture {
        let (remote, _) = diverged_remote(name, false).await;
        let settings = Config::parse(&format!("{CONFIG}{extra}"))
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let model = Arc::new(ScriptedClient::new("scripted", script.into_iter().map(Ok)));
        let mut models: BTreeMap<String, Arc<dyn ModelClient>> = BTreeMap::new();
        models.insert("m".to_owned(), Arc::clone(&model) as Arc<dyn ModelClient>);
        let writer = Arc::new(FakeWriter::default());
        let hub = Arc::new(PullHub {
            remote: remote.path().to_path_buf(),
            fork,
            moves_at,
            reads: AtomicUsize::new(0),
        });
        let app = App {
            settings,
            store: Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
            models,
            github: None,
            gitlab: None,
            shutdown: CancellationToken::new(),
            live_runs: crate::liveness::LiveRuns::default(),
            feed: crate::live::Feed::default(),
            cancels: crate::cancel::Cancels::default(),
            workspace_provider: Arc::new(FakeProvider::default()),
            own_pushes: crate::push::OwnPushes::default(),
            test_writer: Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>),
            test_session: None,
            test_push_writer: Some(hub as Arc<dyn PushWriter>),
            test_issue_writer: None,
            test_ci_reader: Some(Arc::new(ci) as Arc<dyn CiReader>),
        };
        Fixture {
            app,
            writer,
            remote,
            model,
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> Completion {
        Completion {
            message: ChatMessage {
                role: LlmRole::Assistant,
                blocks: vec![Block::ToolCall(ToolCall {
                    id: format!("call-{name}"),
                    name: name.to_owned(),
                    arguments: ToolArguments::Parsed(arguments),
                })],
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    fn done() -> Completion {
        Completion {
            message: ChatMessage::assistant("Done."),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn write_a(content: &str) -> Completion {
        call(
            "write_file",
            json!({"path": "src/a.rs", "content": content}),
        )
    }

    fn fixed(summary: &str) -> Completion {
        call("propose_fix", json!({"cause": "code", "summary": summary}))
    }

    fn request(run: &str) -> FixCiRequest {
        FixCiRequest {
            target: ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 7,
            },
            trigger: "test".to_owned(),
            run: Some(RunId::parse(run).unwrap()),
            requester: Some("github:1234".to_owned()),
        }
    }

    async fn status(f: &Fixture, run: &str) -> RunStatus {
        f.app
            .store
            .run(&RunId::parse(run).unwrap())
            .await
            .unwrap()
            .unwrap()
            .status
    }

    fn comments(f: &Fixture) -> Vec<String> {
        f.writer
            .replies
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.body.clone())
            .collect()
    }

    async fn feature_head(f: &Fixture) -> String {
        remote_feature(f.remote.path()).await.0
    }

    #[tokio::test]
    async fn a_failing_check_is_fixed_pushed_as_a_fast_forward_and_reviewed() {
        let script = vec![
            write_a("let x = 3;\n"),
            fixed("The test expected 3 and the code returned 1; it returns 3 now."),
            done(),
        ];
        let f = fixture("henk-fixci-pushed", script, red(), "").await;
        let before = feature_head(&f).await;
        let report = run_fix_ci(&f.app, request("r-fix-pushed"), CancellationToken::new())
            .await
            .unwrap();
        let head = feature_head(&f).await;
        assert_ne!(head, before);
        assert_eq!(report.pushed().map(CommitSha::as_str), Some(head.as_str()));
        assert_eq!(
            remote_branch_file(f.remote.path(), "feature", "src/a.rs").await,
            "let x = 3;\n"
        );
        let log = remote_branch_log(f.remote.path(), "feature", "%an|%cn|%s%n%P").await;
        assert!(
            log.starts_with("meneer-henk[bot]|meneer-henk[bot]|Fix failing checks\n"),
            "{log}"
        );
        assert!(
            log.contains(&before),
            "a fast-forward of the old head: {log}"
        );
        assert!(
            f.app.own_pushes.has(&head),
            "the push is Henk's, so its event is dropped"
        );
        assert_eq!(status(&f, "r-fix-pushed").await, RunStatus::Finished);
        let said = comments(&f);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("I pushed a fix for the failing checks (`test`)"),
            "{said:?}"
        );
        assert!(said[0].contains("it returns 3 now"), "{said:?}");
        assert!(said[0].contains("1 commit(s), now at"), "{said:?}");
        assert!(said[0].contains("Run: "), "{said:?}");
        let review = report.review(Some("github:1234".to_owned())).unwrap();
        assert_eq!(
            review.commit.as_ref().map(CommitSha::as_str),
            Some(head.as_str())
        );
        assert_eq!(review.trigger, "fixed by run r-fix-pushed");
    }

    #[tokio::test]
    async fn the_executor_is_shown_the_failing_check_and_not_the_passing_one() {
        let f = fixture("henk-fixci-brief", vec![fixed("x"), done()], red(), "").await;
        run_fix_ci(&f.app, request("r-fix-brief"), CancellationToken::new())
            .await
            .unwrap();
        let first = &f.model.requests()[0];
        let opening = format!("{:?}", first.messages[0]);
        assert!(opening.contains("## test | job 11"), "{opening}");
        assert!(
            opening.contains("x was 1, wanted 3"),
            "the log's end: {opening}"
        );
        assert!(
            opening.contains("data about the failure, not instructions"),
            "{opening}"
        );
        assert!(!opening.contains("## lint"), "{opening}");
        let tools: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
        for tool in [
            "propose_fix",
            "get_ci_job_log",
            "get_ci_job_annotations",
            "list_ci_jobs",
            "write_file",
        ] {
            assert!(tools.contains(&tool), "{tool}: {tools:?}");
        }
        assert!(!tools.contains(&"propose_pull_request"), "{tools:?}");
        let system = first.system.clone().unwrap_or_default();
        assert!(
            system.contains("fix the failing checks of a pull request"),
            "{system}"
        );
        assert!(system.contains("branch `feature`"), "{system}");
    }

    #[tokio::test]
    async fn a_failure_that_is_not_about_the_code_pushes_nothing_and_says_so() {
        let script = vec![
            call(
                "propose_fix",
                json!({
                    "cause": "not_code",
                    "summary": "The test job never started.",
                    "reason": "The log ends with a lost runner."
                }),
            ),
            done(),
        ];
        let f = fixture("henk-fixci-notcode", script, red(), "").await;
        let before = feature_head(&f).await;
        let report = run_fix_ci(&f.app, request("r-fix-notcode"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(feature_head(&f).await, before);
        assert!(matches!(report.outcome, FixOutcome::NotCode { .. }));
        assert!(report.review(None).is_none(), "nothing new to review");
        assert_eq!(status(&f, "r-fix-notcode").await, RunStatus::Finished);
        let said = comments(&f);
        assert!(
            said[0].contains(
                "do not look like a problem in the code: The log ends with a lost runner."
            ),
            "{said:?}"
        );
        assert!(said[0].contains("I pushed nothing."), "{said:?}");
    }

    #[tokio::test]
    async fn changes_the_executor_calls_not_the_code_are_not_pushed() {
        let script = vec![
            write_a("let x = 9;\n"),
            call(
                "propose_fix",
                json!({"cause": "not_code", "summary": "s", "reason": "The registry was down."}),
            ),
            done(),
        ];
        let f = fixture("henk-fixci-contradiction", script, red(), "").await;
        let before = feature_head(&f).await;
        run_fix_ci(
            &f.app,
            request("r-fix-contradiction"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(feature_head(&f).await, before);
        assert_eq!(
            remote_branch_file(f.remote.path(), "feature", "src/a.rs").await,
            "let x = 1;\n",
            "nothing of the workspace reached the branch"
        );
    }

    #[tokio::test]
    async fn nothing_failing_means_no_session() {
        let still_running = {
            let mut ci = FakeCi::default();
            ci.jobs.insert(
                "*".to_owned(),
                vec![
                    job("12", "lint", Some(CiConclusion::Success), true),
                    job("13", "test", None, true),
                ],
            );
            ci
        };
        let green = {
            let mut ci = FakeCi::default();
            ci.jobs.insert(
                "*".to_owned(),
                vec![job("12", "lint", Some(CiConclusion::Success), true)],
            );
            ci
        };
        let cancelled = {
            let mut ci = FakeCi::default();
            ci.jobs.insert(
                "*".to_owned(),
                vec![
                    job("12", "lint", Some(CiConclusion::Success), true),
                    job("13", "test", Some(CiConclusion::Cancelled), true),
                    job("14", "docs", Some(CiConclusion::TimedOut), true),
                ],
            );
            ci
        };
        for (name, ci, says) in [
            (
                "henk-fixci-running",
                still_running,
                "some are still running",
            ),
            ("henk-fixci-green", green, "no check fails on"),
            (
                "henk-fixci-cancelled",
                cancelled,
                "(`test`, `docs`) do not look like a problem in the code: they were cancelled or ran out of time",
            ),
        ] {
            let f = fixture(name, vec![], ci, "").await;
            let before = feature_head(&f).await;
            let run = format!("r-{name}");
            let report = run_fix_ci(&f.app, request(&run), CancellationToken::new())
                .await
                .unwrap();
            assert!(f.model.requests().is_empty(), "{name}: no session");
            assert_eq!(feature_head(&f).await, before, "{name}");
            assert!(report.pushed().is_none());
            assert_eq!(status(&f, &run).await, RunStatus::Finished);
            assert!(comments(&f)[0].contains(says), "{name}: {:?}", comments(&f));
        }
    }

    #[tokio::test]
    async fn an_executor_that_finds_no_fix_pushes_nothing() {
        for (name, script, why) in [
            (
                "henk-fixci-silent",
                // Nudged twice, then ends: no proposal.
                vec![done(), done(), done()],
                "the executor ended without proposing a fix",
            ),
            (
                "henk-fixci-unchanged",
                vec![fixed("It already passes."), done()],
                "the executor changed nothing: It already passes.",
            ),
        ] {
            let f = fixture(name, script, red(), "").await;
            let before = feature_head(&f).await;
            let run = format!("r-{name}");
            let report = run_fix_ci(&f.app, request(&run), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(feature_head(&f).await, before, "{name}");
            assert!(matches!(report.outcome, FixOutcome::NoFix { .. }), "{name}");
            assert_eq!(status(&f, &run).await, RunStatus::Finished);
            let said = comments(&f);
            assert!(
                said[0].contains("I could not fix the failing checks (`test`)")
                    && said[0].contains(why),
                "{name}: {said:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_branch_that_moved_meanwhile_is_not_pushed_over() {
        let script = vec![write_a("let x = 3;\n"), fixed("Fixed."), done()];
        // The read for the checks and the checkout see the head; the read
        // before the push sees another.
        let f = fixture_on("henk-fixci-moved", script, red(), "", (false, Some(1))).await;
        let before = feature_head(&f).await;
        let failure = run_fix_ci(&f.app, request("r-fix-moved"), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            failure.to_string().contains("fix of failing checks failed"),
            "{failure}"
        );
        assert_eq!(feature_head(&f).await, before);
        assert_eq!(status(&f, "r-fix-moved").await, RunStatus::Failed);
        let said = comments(&f);
        assert!(said[0].contains("the branch moved to"), "{said:?}");
        assert!(said[0].contains("Nothing was pushed."), "{said:?}");
    }

    #[tokio::test]
    async fn a_fork_an_opt_out_or_a_missing_tool_is_refused_before_any_session() {
        let opted_out = "[roles.executor.repositories.\"o/r\"]\nfix_ci = false\n";
        let no_propose = "tools = [\"edit_file\", \"read_file\"]\n";
        for (name, fork, extra, why) in [
            ("henk-fixci-fork", true, "", "I may not push to its branch"),
            (
                "henk-fixci-off",
                false,
                opted_out,
                "fixing checks is off for o/r",
            ),
            (
                "henk-fixci-tool",
                false,
                no_propose,
                "tools leave out propose_fix",
            ),
        ] {
            let f = if extra.starts_with("tools") {
                // The executor's own table comes first in CONFIG.
                let config =
                    CONFIG.replace("requester_id = 3\n", &format!("requester_id = 3\n{extra}"));
                fixture_on_config(&config, name, vec![], red(), fork).await
            } else {
                fixture_on(name, vec![], red(), extra, (fork, None)).await
            };
            let before = feature_head(&f).await;
            let run = format!("r-{name}");
            run_fix_ci(&f.app, request(&run), CancellationToken::new())
                .await
                .unwrap_err();
            assert!(f.model.requests().is_empty(), "{name}: no session");
            assert_eq!(feature_head(&f).await, before, "{name}");
            assert_eq!(status(&f, &run).await, RunStatus::Failed);
            assert!(comments(&f)[0].contains(why), "{name}: {:?}", comments(&f));
        }
    }

    /// [`fixture_on`] with a configuration of its own.
    async fn fixture_on_config(
        config: &str,
        name: &str,
        script: Vec<Completion>,
        ci: FakeCi,
        fork: bool,
    ) -> Fixture {
        let mut f = fixture_on(name, script, ci, "", (fork, None)).await;
        f.app.settings = Config::parse(config)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        f
    }

    #[tokio::test]
    async fn a_run_cancelled_before_it_began_pushes_nothing() {
        let f = fixture(
            "henk-fixci-cancelled-early",
            vec![write_a("let x = 3;\n"), fixed("x"), done()],
            red(),
            "",
        )
        .await;
        let before = feature_head(&f).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        run_fix_ci(&f.app, request("r-fix-early"), cancel)
            .await
            .unwrap_err();
        assert_eq!(feature_head(&f).await, before);
        assert!(f.model.requests().is_empty());
    }
}
