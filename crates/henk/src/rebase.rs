//! The rebase run (§3.6, #579): Henk replays a pull request's commits onto
//! its base branch and pushes the result over the branch, with a lease on
//! the head he read, so a commit pushed meanwhile is never lost. When a
//! commit stops on conflicts, the executor resolves them in a workspace;
//! Henk's code checks that only the conflicted files changed and no marker
//! is left, and goes on. Anything else stops the rebase and nothing is
//! pushed.
//!
//! Every rebase run ends with one comment on the pull request: what was
//! rebased, that it was already up to date, or why it could not be. A
//! rebased head is then reviewed like any other, in a run of its own; the
//! coordinator asks for it, since the platform's event about Henk's own
//! push is dropped as his own (§8.1).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, StopCause, prompts};
use henk_domain::push::push_refusal;
use henk_domain::review::CommitSha;
use henk_domain::role::Role;
use henk_domain::run::{RunId, RunKind};
use henk_llm::ChatMessage;
use henk_platform::ReviewTarget;
use henk_platform::push::{CommitIdentity, PullFacts, PushWriter};
use henk_session::{SessionSpec, model_id, run_session};
use henk_store::{NewRun, RunStatus, Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::app::App;
use crate::cancel::Cancelled;
use crate::config::{EditFormat, ExecutorConfig, ModelFileConfig};
use crate::edit_tools::{EditContext, EditState, edit_tools};
use crate::git::{Checkout, RebaseStep, ScratchDir};
use crate::ids::new_run_id;
use crate::issue_runs::{OnRun, PullThread};
use crate::liveness::KeepAlive;
use crate::push::{Pusher, stop_if_cancelled};
use crate::review::ReviewRequest;
use crate::roles::equip_and_grant;
use crate::stages;
use crate::workspace::{checked_changeset, open_prepared};

/// How long one rebase run may take, conflicts and all.
const RUN_TIMEOUT: Duration = Duration::from_mins(30);

/// The most commits of one rebase that may stop on conflicts: past this,
/// the pull request is better rebased by a person.
const MAX_CONFLICT_STEPS: usize = 10;

/// A request to rebase one pull request onto its base branch.
#[derive(Debug, Clone)]
pub struct RebaseRequest {
    /// The pull request.
    pub target: ReviewTarget,
    /// What started it.
    pub trigger: String,
    /// The run id to use, when the caller already announced one.
    pub run: Option<RunId>,
    /// Who asked, for the run record: a stable id such as `github:<id>`,
    /// a commenter's login, or nobody.
    pub requester: Option<String>,
}

/// How a rebase run ended well.
#[derive(Debug)]
pub struct RebaseReport {
    /// The run.
    pub run: RunId,
    /// The pull request.
    pub target: ReviewTarget,
    /// The head Henk pushed, when he pushed one; `None` when it was
    /// already up to date.
    pub pushed: Option<CommitSha>,
}

impl RebaseReport {
    /// The review of the rebased head, when there is one: a run of its own
    /// at the commit Henk pushed. `requester` is who asked for the rebase.
    #[must_use]
    pub fn review(&self, requester: Option<String>) -> Option<ReviewRequest> {
        let commit = self.pushed.clone()?;
        Some(ReviewRequest {
            target: self.target.clone(),
            commit: Some(commit),
            trigger: format!("rebased by run {}", self.run),
            requester,
            acknowledge: None,
            run: None,
            submitted_at: None,
            queued_check: None,
        })
    }
}

/// What a rebase did.
enum Rebased {
    /// The base was already in the branch.
    UpToDate { base: String },
    /// The branch was rebased and pushed.
    Pushed {
        base: String,
        head: CommitSha,
        commits: usize,
        resolved: BTreeSet<String>,
    },
}

impl Rebased {
    /// What Henk says on the pull request.
    fn said(&self) -> String {
        match self {
            Self::UpToDate { base } => {
                format!(
                    "This pull request is already up to date with `{base}`; there was nothing to rebase."
                )
            }
            Self::Pushed {
                base,
                head,
                commits,
                resolved,
            } => {
                let conflicts = if resolved.is_empty() {
                    String::new()
                } else {
                    let files: Vec<String> = resolved.iter().map(|f| format!("`{f}`")).collect();
                    format!(" I resolved conflicts in {}.", files.join(", "))
                };
                format!(
                    "I rebased this pull request onto `{base}`: {commits} commit(s), now at {}.{conflicts} A review of the rebased commits follows.",
                    head.short()
                )
            }
        }
    }

    fn pushed(&self) -> Option<CommitSha> {
        match self {
            Self::UpToDate { .. } => None,
            Self::Pushed { head, .. } => Some(head.clone()),
        }
    }
}

/// Rebases one pull request onto its base branch, end to end. Every
/// rebase run ends with one comment on the pull request.
#[instrument(skip_all, fields(repo = %request.target.repo.path(), pull = request.target.number))]
pub async fn run_rebase(
    app: &App,
    request: RebaseRequest,
    cancel: CancellationToken,
) -> anyhow::Result<RebaseReport> {
    let executor = app
        .settings
        .roles
        .executor
        .as_ref()
        .ok_or_else(|| anyhow!("rebasing needs [roles.executor]"))?;
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
            kind: RunKind::Rebase,
            platform: target.repo.platform(),
            repo: target.repo.path(),
            target: target.number,
            commit: None,
            requester: request.requester.clone(),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await?;
    info!(run = %run, "rebase started");
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
        what: "rebase",
    };
    match result {
        Ok(rebased) => {
            let said = rebased.said();
            on_pull.post(&said).await;
            stages::finished(&*app.store, &run, &said).await;
            app.store
                .finish_run(&run, RunStatus::Finished, Some(&said), None)
                .await?;
            info!(run = %run, "rebase ended");
            Ok(RebaseReport {
                run,
                target: target.clone(),
                pushed: rebased.pushed(),
            })
        }
        Err(failure) => {
            if let Some(ended) = on_pull.ended_early(&failure).await {
                return Err(ended);
            }
            let reason = format!("{failure:#}");
            let comment =
                format!("I could not rebase this pull request: {reason}. Nothing was pushed.");
            Err(on_pull.failed(&comment, &reason).await)
        }
    }
}

/// One rebase run's work, from reading the pull request to the push.
struct Work<'a> {
    app: &'a App,
    executor: &'a ExecutorConfig,
    request: &'a RebaseRequest,
    run: &'a RunId,
    model: Arc<dyn henk_llm::ModelClient>,
    deadline: tokio::time::Instant,
}

impl Work<'_> {
    async fn run(&self, cancel: &CancellationToken) -> anyhow::Result<Rebased> {
        let (store, run, target) = (&*self.app.store, self.run, &self.request.target);
        stages::requested(
            store,
            run,
            None,
            &self.request.trigger,
            self.request.requester.as_deref(),
        )
        .await;
        let what = format!("rebasing #{}", target.number);
        stages::mark(store, run, Stage::Started, StageState::Done, what).await;
        // Cancelled while it waited for a slot (#580): it ends here.
        if cancel.is_cancelled() {
            return Err(Cancelled.into());
        }
        if !self.executor.rebases(&target.repo) {
            return Err(anyhow!(
                "rebasing is off for {} ([roles.executor.repositories] rebase = false)",
                target.repo.path()
            ));
        }

        stages::mark(store, run, Stage::Checkout, StageState::Running, "").await;
        let writer = self.app.push_writer(&target.repo)?;
        let checked_out = tokio::select! {
            checked_out = self.checkout(&*writer) => checked_out,
            () = cancel.cancelled() => return Err(Cancelled.into()),
        };
        let (facts, checkout) = match checked_out {
            Ok(checked_out) => checked_out,
            Err(error) => {
                let why = format!("{error:#}");
                stages::mark(store, run, Stage::Checkout, StageState::Failed, &why).await;
                return Err(error);
            }
        };
        let ready = format!(
            "{} at {}, onto {}",
            facts.push.head_ref,
            facts.head.short(),
            facts.base_ref
        );
        stages::mark(store, run, Stage::Checkout, StageState::Done, ready).await;

        let pusher = Pusher {
            app: self.app,
            writer: Arc::clone(&writer),
            repo: &target.repo,
            run,
            requester: None,
        };
        let committer = pusher.committer().await?;
        stages::mark(store, run, Stage::Rebase, StageState::Running, "").await;
        let replayed = self.replay(&checkout, &facts, &committer, cancel).await;
        let resolved = match replayed {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                let base = facts.base_ref.clone();
                let done = format!("already up to date with {base}");
                stages::mark(store, run, Stage::Rebase, StageState::Done, done).await;
                return Ok(Rebased::UpToDate { base });
            }
            Err(error) => {
                // Whatever stopped it, the branch is left as it was.
                let _ = checkout.rebase_abort().await;
                let why = format!("{error:#}");
                stages::mark(store, run, Stage::Rebase, StageState::Failed, &why).await;
                return Err(error);
            }
        };
        let commits = checkout
            .commits_ahead(&facts.base_ref)
            .await
            .context("counting the rebased commits")?;
        let done = format!("{commits} commit(s) onto {}", facts.base_ref);
        stages::mark(store, run, Stage::Rebase, StageState::Done, done).await;
        stop_if_cancelled(cancel)?;

        stages::mark(store, run, Stage::Push, StageState::Running, "").await;
        let rebased_head = pusher
            .push_rebased(&checkout, target, &facts, cancel)
            .await
            .and_then(|head| CommitSha::parse(&head).context("the rebased head"));
        let head = match rebased_head {
            Ok(head) => head,
            Err(error) => {
                let why = format!("{error:#}");
                stages::mark(store, run, Stage::Push, StageState::Failed, &why).await;
                return Err(error);
            }
        };
        stages::mark(store, run, Stage::Push, StageState::Done, head.short()).await;
        Ok(Rebased::Pushed {
            base: facts.base_ref,
            head,
            commits,
            resolved,
        })
    }

    /// The pull request as it is now, once Henk may push to it, and a
    /// checkout of its branch with its base beside it.
    async fn checkout(&self, writer: &dyn PushWriter) -> anyhow::Result<(PullFacts, Checkout)> {
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
        if facts.base_ref.is_empty() {
            return Err(anyhow!(
                "the platform did not say which branch it merges into"
            ));
        }
        let credential = writer
            .git_credential()
            .await
            .context("getting a credential for git")?;
        let dir = ScratchDir::new(&format!("henk-rebase-{}", self.run))
            .context("making the checkout directory")?;
        let checkout = Checkout::clone_for_rebase(
            dir,
            &facts.remote,
            &facts.push.head_ref,
            &facts.head,
            &facts.base_ref,
            credential,
        )
        .await
        .context("checking out the branch and its base")?;
        Ok((facts, checkout))
    }

    /// Replays the branch onto its base, resolving each commit's conflicts
    /// with the executor. `None` when it was already up to date; else the
    /// files whose conflicts were resolved.
    async fn replay(
        &self,
        checkout: &Checkout,
        facts: &PullFacts,
        committer: &CommitIdentity,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Option<BTreeSet<String>>> {
        let base = &facts.base_ref;
        let mut step = checkout.rebase(committer, base).await.context("rebasing")?;
        let mut resolved = BTreeSet::new();
        let mut conflicts = 0;
        loop {
            match step {
                RebaseStep::UpToDate => return Ok(None),
                RebaseStep::Done => return Ok(Some(resolved)),
                RebaseStep::Conflict { commit, files } => {
                    conflicts += 1;
                    if conflicts > MAX_CONFLICT_STEPS {
                        return Err(anyhow!(
                            "more than {MAX_CONFLICT_STEPS} commits stop on conflicts; this one is better rebased by hand"
                        ));
                    }
                    stop_if_cancelled(cancel)?;
                    self.resolve(checkout, base, &commit, &files, conflicts, cancel)
                        .await
                        .with_context(|| {
                            format!("replaying \"{commit}\" conflicts in {}", files.join(", "))
                        })?;
                    resolved.extend(files.iter().cloned());
                    step = checkout
                        .rebase_continue(committer, &files)
                        .await
                        .context("going on with the rebase")?;
                }
            }
        }
    }

    /// Has the executor resolve the conflicts of the `nth` commit that stops
    /// on one, in a workspace of the checkout, and takes its changes back once
    /// they are only to the conflicted files and leave no marker.
    async fn resolve(
        &self,
        checkout: &Checkout,
        base: &str,
        commit: &str,
        files: &[String],
        nth: usize,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let (app, store, run) = (self.app, &*self.app.store, self.run);
        let repo = &self.request.target.repo;
        let doing = format!("resolving conflicts in {}", files.join(", "));
        stages::mark(store, run, Stage::Session, StageState::Running, &doing).await;
        let profile = app.settings.workspace.profile_for(&repo.path());
        let workspace = open_prepared(app, run, checkout, profile, Some(self.deadline))
            .await
            .context("opening a workspace for the conflicts")?;
        let editing = Arc::new(EditContext {
            workspace: Arc::clone(&workspace),
            command_timeout: Duration::from_secs(profile.limits.command_secs),
            max_changed_files: files.len(),
            // Resolving a conflict needs no commands of its own.
            bash: false,
            edit_format: app
                .settings
                .models
                .get(&self.executor.model)
                .map_or(EditFormat::Replace, ModelFileConfig::edit_format),
            state: Mutex::new(EditState::default()),
        });
        let spec = self.session(&editing, base, commit, files, nth);
        let outcome = run_session(store, run, spec, cancel.clone()).await;
        let result = match session_result(outcome.stop) {
            Ok(()) => workspace.export().await.context("listing the resolutions"),
            Err(error) => Err(error),
        };
        workspace.close().await;
        let changes = checked_changeset(result?, files.len())
            .map_err(|why| anyhow!("the resolutions cannot be taken: {why}"))?;
        if let Some(outside) = changes
            .iter()
            .find(|change| !files.iter().any(|f| f == change.path.as_str()))
        {
            return Err(anyhow!(
                "the executor changed {}, which had no conflict",
                outside.path
            ));
        }
        checkout.apply(&changes).context("taking the resolutions")?;
        let left = checkout.still_conflicted(files);
        if !left.is_empty() {
            return Err(anyhow!("conflict markers are left in {}", left.join(", ")));
        }
        stages::mark(store, run, Stage::Session, StageState::Done, "resolved").await;
        Ok(())
    }

    /// The executor's session for the conflicts of the `nth` commit that
    /// stops on one.
    fn session(
        &self,
        editing: &Arc<EditContext>,
        base: &str,
        commit: &str,
        files: &[String],
        nth: usize,
    ) -> SessionSpec {
        let target = &self.request.target;
        let listed: Vec<String> = files.iter().map(|f| format!("- `{f}`")).collect();
        let mut system = format!(
            "{}\n\n{}",
            prompts::PERSONA,
            prompts::render(
                prompts::REBASE,
                &[
                    ("ref", &format!("#{}", target.number)),
                    ("repo", &target.repo.path()),
                    ("base", base),
                    ("commit", commit),
                    ("files", &listed.join("\n")),
                    ("edit_tool", editing.edit_format.tool()),
                ],
            )
        );
        let tools = equip_and_grant(
            Role::Executor,
            &mut system,
            edit_tools(editing),
            self.app.settings.skills.select(&self.executor.skills),
            &self.executor.tools(),
        );
        let opening = format!(
            "Resolve the conflicts in {}.",
            files
                .iter()
                .map(|f| format!("`{f}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let settings = &self.app.settings;
        SessionSpec {
            name: conflict_session_name(nth),
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
            continuation: None,
            turn_warning: None,
        }
    }
}

/// The name of the session that resolves the `nth` (1-based) commit's
/// conflicts. A run's sessions are unique by name (#630), and a branch whose
/// commits conflict one after the other has one session for each.
fn conflict_session_name(nth: usize) -> String {
    match nth {
        0 | 1 => "executor".to_owned(),
        n => format!("executor-{n}"),
    }
}

/// Whether a conflict session ended in a way its changes can be taken from.
fn session_result(stop: StopCause) -> anyhow::Result<()> {
    match stop {
        StopCause::EndTurn | StopCause::MaxTurns => Ok(()),
        StopCause::Timeout => Err(anyhow!(
            "the time limit of {}s was reached",
            RUN_TIMEOUT.as_secs()
        )),
        StopCause::Cancelled => Err(Cancelled.into()),
        StopCause::ModelError(error) => Err(anyhow!("model error: {error}")),
        StopCause::Refused(why) => Err(anyhow!("the model declined the work ({why})")),
        StopCause::Stuck { tool, .. } => Err(anyhow!(
            "the model kept repeating {tool} with the same arguments"
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::collections::BTreeMap;

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::push::PushFacts;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{
        Block, Completion, ModelClient, Role as LlmRole, StopReason, ToolArguments, ToolCall, Usage,
    };
    use henk_platform::push::{BehindPull, GitCredential, NewPull, OpenedPull, RepoHead};
    use henk_platform::{PlatformError, PlatformWriter};
    use henk_store::{RunStatus, SessionStatus};
    use serde_json::json;

    use super::*;
    use crate::config::Config;
    use crate::git::tests::{
        diverged_remote, diverged_remote_clashing, remote_branch_file, remote_branch_log,
        remote_feature,
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

    /// A pull request whose branch `feature` is on a bare remote beside
    /// its base `main`; `fork` says its branch is in another repository.
    struct PullHub {
        remote: std::path::PathBuf,
        fork: bool,
    }

    fn unused() -> PlatformError {
        PlatformError::Decode("not used by a rebase".to_owned())
    }

    #[async_trait::async_trait]
    impl PushWriter for PullHub {
        async fn pull_facts(&self, _: &ReviewTarget) -> Result<PullFacts, PlatformError> {
            let (head, _) = remote_feature(&self.remote).await;
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

    /// An app whose executor follows `script`, for a pull request on a
    /// fresh remote `name` (see [`diverged_remote`]), under `extra` config.
    async fn fixture(
        name: &str,
        flags: (bool, bool),
        extra: &str,
        script: Vec<Completion>,
    ) -> Fixture {
        fixture_on(CONFIG, name, flags, extra, script).await
    }

    /// [`fixture`] on a configuration of its own.
    async fn fixture_on(
        config: &str,
        name: &str,
        (clash, fork): (bool, bool),
        extra: &str,
        script: Vec<Completion>,
    ) -> Fixture {
        let (remote, _) = diverged_remote(name, clash).await;
        fixture_of(config, remote, fork, extra, script)
    }

    /// [`fixture`] for a branch with `clashes` commits that stop on a
    /// conflict (see [`diverged_remote_clashing`]).
    async fn fixture_clashing(name: &str, clashes: usize, script: Vec<Completion>) -> Fixture {
        let (remote, _) = diverged_remote_clashing(name, clashes).await;
        fixture_of(CONFIG, remote, false, "", script)
    }

    fn fixture_of(
        config: &str,
        remote: ScratchDir,
        fork: bool,
        extra: &str,
        script: Vec<Completion>,
    ) -> Fixture {
        let settings = Config::parse(&format!("{config}{extra}"))
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let model = Arc::new(ScriptedClient::new("scripted", script.into_iter().map(Ok)));
        let mut models: BTreeMap<String, Arc<dyn ModelClient>> = BTreeMap::new();
        models.insert("m".to_owned(), Arc::clone(&model) as Arc<dyn ModelClient>);
        let writer = Arc::new(FakeWriter::default());
        let hub = Arc::new(PullHub {
            remote: remote.path().to_path_buf(),
            fork,
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
            test_ci_reader: None,
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

    fn request(run: &str) -> RebaseRequest {
        RebaseRequest {
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
    async fn a_branch_behind_its_base_is_rebased_pushed_and_reviewed() {
        let f = fixture("henk-rebase-clean", (false, false), "", vec![]).await;
        let report = run_rebase(&f.app, request("r-rebase-clean"), CancellationToken::new())
            .await
            .unwrap();
        let head = feature_head(&f).await;
        assert_eq!(
            report.pushed.as_ref().map(CommitSha::as_str),
            Some(head.as_str())
        );
        let log = remote_branch_log(f.remote.path(), "feature", "%an|%cn|%s%n%P").await;
        assert!(log.starts_with("Seed|meneer-henk[bot]|Add b\n"), "{log}");
        let main = remote_branch_log(f.remote.path(), "main", "%H").await;
        assert!(
            log.contains(main.trim()),
            "the parent is main's head: {log}"
        );
        assert_eq!(status(&f, "r-rebase-clean").await, RunStatus::Finished);
        let said = comments(&f);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("I rebased this pull request onto `main`: 1 commit(s)"),
            "{said:?}"
        );
        assert!(said[0].contains("Run: "), "{said:?}");
        let review = report.review(Some("github:1234".to_owned())).unwrap();
        assert_eq!(
            review.commit.as_ref().map(CommitSha::as_str),
            Some(head.as_str())
        );
        assert_eq!(review.trigger, "rebased by run r-rebase-clean");

        let again = run_rebase(&f.app, request("r-rebase-again"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(again.pushed, None);
        assert!(again.review(None).is_none(), "nothing new to review");
        assert_eq!(feature_head(&f).await, head);
        assert!(comments(&f)[1].contains("already up to date with `main`"));
    }

    #[tokio::test]
    async fn the_executor_resolves_a_conflict_and_the_rebase_is_pushed() {
        let script = vec![
            call(
                "write_file",
                json!({"path": "src/a.rs", "content": "let x = 3;\n"}),
            ),
            done(),
        ];
        let f = fixture("henk-rebase-resolved", (true, false), "", script).await;
        let report = run_rebase(
            &f.app,
            request("r-rebase-resolved"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(report.pushed.is_some());
        assert_eq!(
            remote_branch_file(f.remote.path(), "feature", "src/a.rs").await,
            "let x = 3;\n"
        );
        assert!(
            comments(&f)[0].contains("I resolved conflicts in `src/a.rs`."),
            "{:?}",
            comments(&f)
        );
    }

    /// #630: a branch whose commits conflict one after the other has a
    /// session for each, named so that the run records every start.
    #[tokio::test]
    async fn every_commit_that_conflicts_has_a_session_of_its_own() {
        let script = vec![
            call(
                "write_file",
                json!({"path": "src/a.rs", "content": "let x = 3;\n"}),
            ),
            done(),
            call(
                "write_file",
                json!({"path": "src/c2.rs", "content": "let c2 = 3;\n"}),
            ),
            done(),
        ];
        let f = fixture_clashing("henk-rebase-two-clashes", 2, script).await;
        let report = run_rebase(
            &f.app,
            request("r-rebase-two-clashes"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(report.pushed.is_some());

        let run = RunId::parse("r-rebase-two-clashes").unwrap();
        let sessions = f.app.store.sessions(&run).await.unwrap();
        let names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["executor", "executor-2"], "{sessions:?}");
        for session in &sessions {
            assert_eq!(session.status, SessionStatus::Finished, "{session:?}");
            assert!(session.turns > 0, "{session:?}");
        }
    }

    #[test]
    fn the_first_conflict_keeps_the_executors_name_and_later_ones_count_up() {
        let names: Vec<String> = (1..=3).map(conflict_session_name).collect();
        assert_eq!(names, ["executor", "executor-2", "executor-3"]);
    }

    #[tokio::test]
    async fn a_gpt_executor_is_given_apply_patch_to_resolve_a_conflict() {
        let config = CONFIG.replace("model = \"scripted\"", "model = \"gpt-5.5\"");
        let f = fixture_on(
            &config,
            "henk-rebase-patch",
            (true, false),
            "",
            vec![done()],
        )
        .await;
        run_rebase(&f.app, request("r-rebase-patch"), CancellationToken::new())
            .await
            .unwrap_err();
        let first = &f.model.requests()[0];
        let tools: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tools.contains(&"apply_patch"), "{tools:?}");
        assert!(!tools.contains(&"edit_file"), "{tools:?}");
        let system = first.system.clone().unwrap_or_default();
        assert!(system.contains("`apply_patch` or `write_file`"), "{system}");
        assert!(!system.contains("`edit_file`"), "{system}");
    }

    #[tokio::test]
    async fn an_unresolved_conflict_pushes_nothing_and_says_why() {
        for (name, script, why) in [
            (
                "henk-rebase-left",
                vec![done()],
                "conflict markers are left in src/a.rs",
            ),
            (
                "henk-rebase-outside",
                vec![
                    call("write_file", json!({"path": "src/c.rs", "content": "x\n"})),
                    done(),
                ],
                "the executor changed src/c.rs, which had no conflict",
            ),
        ] {
            let f = fixture(name, (true, false), "", script).await;
            let before = feature_head(&f).await;
            let run = format!("r-{name}");
            let failure = run_rebase(&f.app, request(&run), CancellationToken::new())
                .await
                .unwrap_err();
            assert!(failure.to_string().contains("rebase failed"), "{failure}");
            assert_eq!(feature_head(&f).await, before, "{name}: nothing pushed");
            assert_eq!(status(&f, &run).await, RunStatus::Failed);
            let said = comments(&f);
            assert!(
                said[0].contains("replaying \"Add b\" conflicts in src/a.rs"),
                "{said:?}"
            );
            assert!(said[0].contains(why), "{name}: {said:?}");
            assert!(said[0].contains("Nothing was pushed."), "{said:?}");
        }
    }

    #[tokio::test]
    async fn a_fork_or_a_repository_that_opted_out_is_not_rebased() {
        let opted_out = "[roles.executor.repositories.\"o/r\"]\nrebase = false\n";
        for (name, fork, extra, why) in [
            ("henk-rebase-fork", true, "", "I may not push to its branch"),
            (
                "henk-rebase-off",
                false,
                opted_out,
                "rebasing is off for o/r",
            ),
        ] {
            let f = fixture(name, (false, fork), extra, vec![]).await;
            let before = feature_head(&f).await;
            run_rebase(
                &f.app,
                request(&format!("r-{name}")),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert_eq!(feature_head(&f).await, before);
            assert!(comments(&f)[0].contains(why), "{name}: {:?}", comments(&f));
        }
    }
}
