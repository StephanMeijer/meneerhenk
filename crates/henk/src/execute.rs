//! The execute run (§4a, #329): the executor takes an issue to a pull
//! request. It works in a workspace on the default branch's head, in one
//! conversation from the issue to its proposal, and runs the repository's
//! own checks with `bash` where it has it (#396). Henk's code then commits
//! the checked changeset, pushes it to a new branch under
//! `[execution] branch_prefix` and opens a ready pull request that closes
//! the issue. The pull request is then reviewed like any other, in a run
//! of its own with fresh sessions (#321); the coordinator asks for it.
//!
//! Every execute run ends: with the pull request opened, or with one
//! comment on the issue saying why there is none, with the run link.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, StopCause, prompts};
use henk_domain::execute::{branch_candidate, branch_name, commit_subject, pull_body};
use henk_domain::identity::RequesterId;
use henk_domain::review::CommitSha;
use henk_domain::role::Role;
use henk_domain::run::{RunId, RunKind};
use henk_llm::ChatMessage;
use henk_platform::push::{NewPull, PushWriter, RepoHead};
use henk_platform::{IssueInfo, IssueTarget, ReviewTarget};
use henk_session::{SessionSpec, model_id, run_session};
use henk_store::{NewRun, RunStatus, Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::app::App;
use crate::cancel::Cancelled;
use crate::config::{ExecutionConfig, ExecutorConfig};
use crate::edit_tools::{EditContext, EditState, edit_tools};
use crate::execute_tools::{Proposal, ProposePullRequest, Proposed, propose_continuation};
use crate::git::{Checkout, ScratchDir};
use crate::ids::new_run_id;
use crate::issue_runs::{OnIssue, open_issue};
use crate::liveness::KeepAlive;
use crate::push::{PushTo, Pusher, stop_if_cancelled};
use crate::review::ReviewRequest;
use crate::roles::equip_and_grant;
use crate::stages;
use crate::workspace::{Workspace, checked_changeset, open_prepared};

/// A request to take one issue to a pull request.
#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    /// The issue.
    pub target: IssueTarget,
    /// A note from the requester, if any.
    pub note: Option<String>,
    /// What started it.
    pub trigger: String,
    /// The run id to use, when the caller already announced one.
    pub run: Option<RunId>,
    /// Who asked, for the run record, when not the executor's Team Lead:
    /// `github:<id>` from the dashboard, `mcp:<token>` over MCP.
    pub requester: Option<String>,
}

/// The pull request an execute run opened, for the review that follows.
#[derive(Debug, Clone)]
pub struct OpenedPullRequest {
    /// The pull request.
    pub target: ReviewTarget,
    /// The commit Henk pushed: its head.
    pub commit: CommitSha,
    /// Where people read it.
    pub url: String,
}

/// How an execute run ended.
#[derive(Debug)]
pub struct ExecuteReport {
    /// The run.
    pub run: RunId,
    /// The pull request it opened.
    pub pull: OpenedPullRequest,
}

impl ExecuteReport {
    /// The review of the pull request this run opened: a run of its own,
    /// at the commit Henk pushed, with fresh sessions (#321). `requester`
    /// is who asked for the execute run.
    #[must_use]
    pub fn review(&self, requester: Option<String>) -> ReviewRequest {
        ReviewRequest {
            target: self.pull.target.clone(),
            commit: Some(self.pull.commit.clone()),
            trigger: format!("opened by execute run {}", self.run),
            requester,
            acknowledge: None,
            run: None,
            submitted_at: None,
            queued_check: None,
        }
    }
}

/// Why an execute run has no pull request, in words for the issue.
#[derive(Debug)]
struct NoPullRequest(String);

impl std::fmt::Display for NoPullRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoPullRequest {}

fn no_pull(why: impl Into<String>) -> anyhow::Error {
    NoPullRequest(why.into()).into()
}

/// The most names an execute run tries for its branch: the issue's own,
/// then with `-2` up to this.
const BRANCH_TRIES: u32 = 10;

/// Takes one issue to a pull request, end to end. Every execute run ends
/// (§4a): either the pull request is open, or a comment on the issue says
/// why not, with the run link.
#[instrument(skip_all, fields(repo = %request.target.repo.path(), issue = request.target.number))]
pub async fn run_execute(
    app: &App,
    request: ExecuteRequest,
    cancel: CancellationToken,
) -> anyhow::Result<ExecuteReport> {
    let (execution, executor) = configured(app)?;
    let platform = request.target.repo.platform();
    let issues = app.issue_writer(&request.target.repo)?;
    if !app.settings.allowlist.allows(&request.target.repo) {
        return Err(anyhow!("{} is not on the allowlist", request.target.repo));
    }
    let issue = open_issue(issues.as_ref(), &request.target, "take on").await?;

    let run = request.run.clone().unwrap_or_else(new_run_id);
    let link = app.settings.run_link(&run);
    app.store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Execute,
            platform,
            repo: request.target.repo.path(),
            target: request.target.number,
            commit: None,
            // Who asked, as given; nobody when the request names nobody, not
            // the executor's Team Lead (#399).
            requester: request.requester.clone(),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await?;
    info!(run = %run, "execution started");
    let _alive = KeepAlive::start(Arc::clone(&app.store), &app.live_runs, run.clone());
    let model = app.model(&executor.model)?;
    let model_name = model_id(model.as_ref());
    let work = Work {
        app,
        execution,
        executor,
        request: &request,
        issue: &issue,
        run: &run,
        link: &link,
        model,
        deadline: tokio::time::Instant::now() + Duration::from_secs(execution.run_timeout_secs),
    };
    let result = work.run(&cancel).await;
    let on_issue = OnIssue {
        app,
        writer: &issues,
        target: &request.target,
        run: &run,
        link: &link,
        model: &model_name,
        what: "execution",
    };
    match result {
        Ok(pull) => {
            let opened = format!("pull request #{} opened: {}", pull.target.number, pull.url);
            stages::mark(&*app.store, &run, Stage::Publish, StageState::Done, &opened).await;
            stages::finished(&*app.store, &run, &opened).await;
            app.store
                .finish_run(&run, RunStatus::Finished, Some(&opened), None)
                .await?;
            info!(run = %run, pull = pull.target.number, "execution ended");
            Ok(ExecuteReport { run, pull })
        }
        Err(failure) => {
            if let Some(ended) = on_issue.ended_early(&failure).await {
                return Err(ended);
            }
            let reason = format!("{failure:#}");
            let comment = if failure.downcast_ref::<NoPullRequest>().is_some() {
                format!("I opened no pull request for this issue: {reason}.")
            } else {
                format!(
                    "I could not take this issue to a pull request: {reason}. That is my failure, not the issue's."
                )
            };
            Err(on_issue.failed(&comment, &reason).await)
        }
    }
}

/// `[execution]` and the executor it needs.
fn configured(app: &App) -> anyhow::Result<(&ExecutionConfig, &ExecutorConfig)> {
    let execution = app
        .settings
        .execution
        .as_ref()
        .ok_or_else(|| anyhow!("execution is not configured"))?;
    let executor = app
        .settings
        .roles
        .executor
        .as_ref()
        .ok_or_else(|| anyhow!("execution needs [roles.executor]"))?;
    Ok((execution, executor))
}

/// One execute run's work, from the checkout to the pull request.
struct Work<'a> {
    app: &'a App,
    execution: &'a ExecutionConfig,
    executor: &'a ExecutorConfig,
    request: &'a ExecuteRequest,
    issue: &'a IssueInfo,
    run: &'a RunId,
    link: &'a str,
    model: Arc<dyn henk_llm::ModelClient>,
    /// When the run's time is up: from when the work began, so a wait for a
    /// sandbox host counts against it (#403).
    deadline: tokio::time::Instant,
}

impl Work<'_> {
    /// Checks out the default branch, has the executor work, and pushes
    /// and opens what it proposed. The workspace is closed on every path.
    async fn run(&self, cancel: &CancellationToken) -> anyhow::Result<OpenedPullRequest> {
        let (store, run, target) = (&*self.app.store, self.run, &self.request.target);
        stages::requested(
            store,
            run,
            None,
            &self.request.trigger,
            self.request.requester.as_deref(),
        )
        .await;
        let what = format!("taking #{} to a pull request", target.number);
        stages::mark(store, run, Stage::Started, StageState::Done, what).await;

        stages::mark(store, run, Stage::Checkout, StageState::Running, "").await;
        let writer = self.app.push_writer(&target.repo)?;
        let opened = tokio::select! {
            opened = self.open(&*writer) => opened,
            () = cancel.cancelled() => return Err(Cancelled.into()),
        };
        let (base, workspace, branch) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let why = format!("{error:#}");
                stages::mark(store, run, Stage::Checkout, StageState::Failed, &why).await;
                if error.downcast_ref::<NoPullRequest>().is_some() {
                    return Err(error);
                }
                return Err(error.context("the executor needs a workspace"));
            }
        };
        let ready = format!(
            "ready at {} on {}, for {branch}",
            base.head.short(),
            base.default_branch
        );
        stages::mark(store, run, Stage::Checkout, StageState::Done, ready).await;
        let result = self.work(&writer, &base, &workspace, &branch, cancel).await;
        workspace.close().await;
        result
    }

    /// The default branch's head, a workspace on it, set up, and the
    /// branch the work will go to, chosen before the session so a run
    /// never works for a branch it cannot push.
    async fn open(
        &self,
        writer: &dyn PushWriter,
    ) -> anyhow::Result<(RepoHead, Arc<dyn Workspace>, String)> {
        let repo = &self.request.target.repo;
        let base = writer
            .repo_head(repo)
            .await
            .context("reading the default branch")?;
        let credential = writer
            .git_credential()
            .await
            .context("getting a credential for git")?;
        let dir = ScratchDir::new(&format!("henk-execute-{}", self.run))
            .context("making the checkout directory")?;
        let checkout = Checkout::clone_at(
            dir,
            &base.remote,
            &base.default_branch,
            &base.head,
            credential,
        )
        .await
        .context("checking out the default branch")?;
        let branch = self.free_branch(&checkout).await?;
        let profile = self.app.settings.workspace.profile_for(&repo.path());
        let workspace =
            open_prepared(self.app, self.run, &checkout, profile, Some(self.deadline)).await?;
        Ok((base, workspace, branch))
    }

    /// The first of the issue's branch names that `origin` does not have:
    /// its own, then with `-2`, `-3` and on, so a run after an earlier
    /// one (a failed push, a pull request that did not open, one that was
    /// closed) takes a new branch and never pushes over one (§3.5). The
    /// push checks again.
    async fn free_branch(&self, checkout: &Checkout) -> anyhow::Result<String> {
        let first = branch_name(
            &self.execution.branch_prefix,
            self.request.target.number,
            &self.issue.title,
        );
        for n in 1..=BRANCH_TRIES {
            let branch = branch_candidate(&first, n);
            if !checkout
                .remote_has_branch(&branch)
                .await
                .context("looking for the branch")?
            {
                return Ok(branch);
            }
        }
        Err(no_pull(format!(
            "the branches {first} to {} already exist",
            branch_candidate(&first, BRANCH_TRIES)
        )))
    }

    /// The executor's session, then the checks, the push and the pull
    /// request.
    async fn work(
        &self,
        writer: &Arc<dyn PushWriter>,
        base: &RepoHead,
        workspace: &Arc<dyn Workspace>,
        branch: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<OpenedPullRequest> {
        let (store, run, target) = (&*self.app.store, self.run, &self.request.target);
        let repo = &target.repo;
        let reference = format!("#{}", target.number);
        let profile = self.app.settings.workspace.profile_for(&repo.path());
        let editing = Arc::new(EditContext {
            workspace: Arc::clone(workspace),
            command_timeout: Duration::from_secs(profile.limits.command_secs),
            max_changed_files: self.executor.max_changed_files,
            // Told of bash only when it has it: the backend runs model
            // commands and [roles.executor] tools grant it.
            bash: profile.backend.runs_model_commands()
                && Role::Executor.grants(&self.executor.tools(), "bash"),
            state: Mutex::new(EditState::default()),
        });
        let proposed: Proposed = Arc::default();
        let session = format!("executor, {}", self.model.model());
        stages::mark(store, run, Stage::Session, StageState::Running, session).await;
        let spec = self.session(&editing, &proposed, &reference, branch);
        let outcome = run_session(store, run, spec, cancel.clone()).await;
        self.session_result(outcome.stop)?;
        stages::mark(store, run, Stage::Session, StageState::Done, "proposed").await;
        let proposal = proposed
            .lock()
            .ok()
            .and_then(|p| p.clone())
            .ok_or_else(|| no_pull("the executor ended without proposing a pull request"))?;

        stages::mark(store, run, Stage::Commit, StageState::Running, "").await;
        let changes = self.checked(workspace, &editing, &proposal).await?;
        let to_push = format!("{} file(s) to push", changes.len());
        stages::mark(store, run, Stage::Commit, StageState::Done, to_push).await;
        stop_if_cancelled(cancel)?;

        self.publish(writer, base, branch, &proposal, &changes, cancel)
            .await
    }

    /// Commits the checked changes to a new branch `branch` off `base`,
    /// pushes it and opens its pull request.
    async fn publish(
        &self,
        writer: &Arc<dyn PushWriter>,
        base: &RepoHead,
        branch: &str,
        proposal: &Proposal,
        changes: &[henk_domain::workspace::Change],
        cancel: &CancellationToken,
    ) -> anyhow::Result<OpenedPullRequest> {
        let (store, run, target) = (&*self.app.store, self.run, &self.request.target);
        let (repo, reference) = (&target.repo, format!("#{}", target.number));
        stages::mark(store, run, Stage::Push, StageState::Running, "").await;
        let requester = self
            .request
            .requester
            .as_deref()
            .and_then(RequesterId::parse);
        let pusher = Pusher {
            app: self.app,
            writer: Arc::clone(writer),
            repo,
            run,
            requester: requester.as_ref(),
        };
        let credential = writer
            .git_credential()
            .await
            .context("getting a credential for git")?;
        let name = format!("henk-execute-{run}-push");
        let checkout = pusher
            .checkout(
                &name,
                &base.remote,
                &base.default_branch,
                &base.head,
                credential,
            )
            .await?;
        let to = PushTo::NewBranch { branch, base };
        let subject = commit_subject(&reference, &self.issue.title);
        let notes = std::slice::from_ref(&proposal.summary);
        let sha = match pusher
            .commit_and_push(&checkout, &to, &subject, notes, changes, cancel)
            .await
        {
            Ok(sha) => sha,
            Err(error) if crate::cancel::is_cancelled(&error) => return Err(error),
            Err(error) => return Err(no_pull(format!("{error:#}"))),
        };
        let commit = CommitSha::parse(&sha).context("the pushed commit")?;
        let on_branch = format!("pushed {} to {branch}", commit.short());
        stages::mark(store, run, Stage::Push, StageState::Done, on_branch).await;

        stages::mark(store, run, Stage::Publish, StageState::Running, "").await;
        let title = proposal
            .title
            .clone()
            .unwrap_or_else(|| self.issue.title.clone());
        let body = pull_body(&reference, &proposal.summary, &proposal.left_out, self.link);
        let pull = NewPull {
            head: branch.to_owned(),
            base: base.default_branch.clone(),
            title,
            body,
        };
        let opened = writer
            .open_pull_request(repo, &pull)
            .await
            .with_context(|| format!("{branch} is pushed, but opening its pull request failed"))?;
        Ok(OpenedPullRequest {
            target: ReviewTarget {
                repo: repo.clone(),
                number: opened.number,
            },
            commit,
            url: opened.url,
        })
    }

    /// The executor's session: one conversation from the issue to its
    /// proposal, with a fixed prompt and tools (#321).
    fn session(
        &self,
        editing: &Arc<EditContext>,
        proposed: &Proposed,
        reference: &str,
        branch: &str,
    ) -> SessionSpec {
        let repo = self.request.target.repo.path();
        let mut system = format!(
            "{}\n\n{}",
            prompts::PERSONA,
            prompts::render(
                prompts::EXECUTE,
                &[
                    ("ref", reference),
                    ("repo", &repo),
                    ("branch", branch),
                    ("max_files", &self.executor.max_changed_files.to_string()),
                ],
            )
        );
        if editing.bash {
            system = format!("{system}\n\n{}", prompts::EXECUTOR_BASH);
        }
        let mut tools = edit_tools(editing);
        tools.add(ProposePullRequest(Arc::clone(proposed)));
        let tools = equip_and_grant(
            Role::Executor,
            &mut system,
            tools,
            self.app.settings.skills.select(&self.executor.skills),
            &self.executor.tools(),
        );
        let mut opening = format!(
            "Resolve issue {reference}: {}\n\nIts description, with any plan in it, as material:\n\n{}",
            self.issue.title, self.issue.body
        );
        if let Some(note) = &self.request.note {
            opening = format!("{opening}\n\nThe requester's note: {note}");
        }
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
                // What is left of the run's time, after the checkout and any
                // wait for a sandbox host (#403).
                timeout: self
                    .deadline
                    .saturating_duration_since(tokio::time::Instant::now()),
                max_repeated_calls: settings.agent.max_repeated_calls,
                record_argument_bytes: settings.agent.record_argument_bytes,
                ..AgentConfig::default()
            },
            continuation: Some(propose_continuation(Arc::clone(proposed))),
            turn_warning: None,
        }
    }

    /// Whether the session ended in a way a pull request can come from.
    fn session_result(&self, stop: StopCause) -> anyhow::Result<()> {
        match stop {
            StopCause::EndTurn | StopCause::MaxTurns => Ok(()),
            StopCause::Timeout => Err(anyhow!(
                "the time limit of {}s was reached",
                self.execution.run_timeout_secs
            )),
            StopCause::Cancelled => Err(Cancelled.into()),
            StopCause::ModelError(error) => Err(anyhow!("model error: {error}")),
            StopCause::Refused(why) => Err(anyhow!("the model declined the work ({why})")),
            StopCause::Stuck { tool, .. } => Err(anyhow!(
                "the model kept repeating {tool} with the same arguments"
            )),
        }
    }

    /// What the executor changed, once the push rules hold: the changeset
    /// Henk commits. Henk runs no checks of his own; the executor ran the
    /// repository's, and the pull request's CI is the final word (#396).
    async fn checked(
        &self,
        workspace: &Arc<dyn Workspace>,
        editing: &EditContext,
        proposal: &Proposal,
    ) -> anyhow::Result<Vec<henk_domain::workspace::Change>> {
        let exported = workspace.export().await.context("listing the changes")?;
        if exported.is_empty() {
            let why = if proposal.left_out.is_empty() {
                "the executor changed nothing".to_owned()
            } else {
                format!("the executor changed nothing: {}", proposal.left_out)
            };
            return Err(no_pull(why));
        }
        checked_changeset(exported, editing.max_changed_files)
            .map_err(|why| no_pull(format!("the executor's changes cannot be pushed: {why}")))
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

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{
        Block, Completion, ModelClient, Role as LlmRole, StopReason, ToolArguments, ToolCall, Usage,
    };
    use henk_platform::IssueWriter;
    use henk_platform::PlatformError;
    use henk_platform::push::{CommitIdentity, GitCredential, OpenedPull, PullFacts};
    use serde_json::json;

    use super::*;
    use crate::config::Config;
    use crate::git::tests::{
        bare_remote, remote_branch_file, remote_branch_log, remote_branches, remote_feature,
    };
    use crate::plan_tools::tests::FakeIssueWriter;
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
[execution]
[roles.executor]
model = "m"
requester_id = 3
"#;

    /// The repository as an execute run sees it: its default branch is
    /// `feature` on a bare remote, and the pull requests it opens are
    /// noted, numbered from 12.
    struct IssueHub {
        remote: std::path::PathBuf,
        opened: Mutex<Vec<NewPull>>,
    }

    fn unused() -> PlatformError {
        PlatformError::Decode("not used by an execute run".to_owned())
    }

    #[async_trait::async_trait]
    impl PushWriter for IssueHub {
        async fn pull_facts(&self, _: &ReviewTarget) -> Result<PullFacts, PlatformError> {
            Err(unused())
        }
        async fn repo_head(&self, _: &RepoRef) -> Result<RepoHead, PlatformError> {
            let (head, _) = remote_feature(&self.remote).await;
            Ok(RepoHead {
                default_branch: "feature".to_owned(),
                head: CommitSha::parse(&head).unwrap(),
                remote: self.remote.to_string_lossy().into_owned(),
            })
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
        async fn open_pull_request(
            &self,
            _: &RepoRef,
            pull: &NewPull,
        ) -> Result<OpenedPull, PlatformError> {
            let mut opened = self.opened.lock().unwrap();
            opened.push(pull.clone());
            Ok(OpenedPull {
                number: 11 + u64::try_from(opened.len()).unwrap(),
                url: "https://github.com/o/r/pull/12".to_owned(),
            })
        }
    }

    struct Fixture {
        app: App,
        tracker: Arc<FakeIssueWriter>,
        hub: Arc<IssueHub>,
        provider: FakeProvider,
        model: Arc<ScriptedClient>,
        _remote: ScratchDir,
    }

    /// An app whose executor follows `script`, on a fresh remote `name`.
    async fn fixture(name: &str, script: Vec<Completion>) -> Fixture {
        let (remote, _) = bare_remote(name).await;
        let settings = Config::parse(CONFIG)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let provider = FakeProvider::default();
        let model = Arc::new(ScriptedClient::new("scripted", script.into_iter().map(Ok)));
        let mut models: BTreeMap<String, Arc<dyn ModelClient>> = BTreeMap::new();
        models.insert("m".to_owned(), Arc::clone(&model) as Arc<dyn ModelClient>);
        let tracker = Arc::new(FakeIssueWriter::new(
            Platform::GitHub,
            None,
            "Runs should export as CSV.",
        ));
        let hub = Arc::new(IssueHub {
            remote: remote.path().to_path_buf(),
            opened: Mutex::default(),
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
            workspace_provider: Arc::new(provider.clone()),
            own_pushes: crate::push::OwnPushes::default(),
            test_writer: None,
            test_session: None,
            test_push_writer: Some(Arc::clone(&hub) as Arc<dyn PushWriter>),
            test_issue_writer: Some(Arc::clone(&tracker) as Arc<dyn IssueWriter>),
            test_ci_reader: None,
        };
        Fixture {
            app,
            tracker,
            hub,
            provider,
            model,
            _remote: remote,
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

    fn write_csv() -> Completion {
        call(
            "write_file",
            json!({"path": "src/csv.rs", "content": "pub fn export() {}\n"}),
        )
    }

    fn propose() -> Completion {
        call(
            "propose_pull_request",
            json!({"summary": "Runs export as CSV through src/csv.rs.", "left_out": "JSON export, which the issue does not ask for."}),
        )
    }

    fn request(run: &str) -> ExecuteRequest {
        ExecuteRequest {
            target: IssueTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 9,
            },
            note: Some("keep it small".to_owned()),
            trigger: "test".to_owned(),
            run: Some(RunId::parse(run).unwrap()),
            requester: Some("github:1234".to_owned()),
        }
    }

    #[tokio::test]
    async fn an_execute_run_nobody_known_asked_for_credits_no_one() {
        let f = fixture("henk-exec-nobody", vec![write_csv(), propose(), done()]).await;
        let report = run_execute(
            &f.app,
            ExecuteRequest {
                requester: None,
                ..request("r-exec-nobody")
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let commit = remote_branch_log(&f.hub.remote, "henk/9-export-runs", "%B").await;
        assert!(commit.contains("Henk-Run: r-exec-nobody"), "{commit}");
        // Not the executor's Team Lead: nobody (#399).
        assert!(!commit.contains("Requested-by"), "{commit}");
        assert!(!commit.contains("Co-authored-by"), "{commit}");
        assert_eq!(report.pull.target.number, 12);
        assert_eq!(record(&f, "r-exec-nobody").await.requester, None);
        let events = f
            .app
            .store
            .events(&RunId::parse("r-exec-nobody").unwrap())
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("nobody Henk knows by id asked")),
            "{events:?}"
        );
    }

    async fn record(f: &Fixture, run: &str) -> henk_store::RunRecord {
        f.app
            .store
            .run(&RunId::parse(run).unwrap())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn an_issue_becomes_a_branch_and_a_pull_request_that_closes_it() {
        let f = fixture("henk-exec-ok", vec![write_csv(), propose(), done()]).await;
        let report = run_execute(&f.app, request("r-exec-1"), CancellationToken::new())
            .await
            .unwrap();

        let remote = &f.hub.remote;
        assert_eq!(
            remote_branches(remote).await,
            ["feature", "henk/9-export-runs"]
        );
        let commit = remote_branch_log(remote, "henk/9-export-runs", "%H%n%an%n%B").await;
        let mut lines = commit.lines();
        assert_eq!(lines.next(), Some(report.pull.commit.as_str()));
        assert_eq!(lines.next(), Some("meneer-henk[bot]"));
        assert_eq!(lines.next(), Some("Resolve #9: Export runs"));
        assert!(
            commit.contains("- Runs export as CSV through src/csv.rs."),
            "{commit}"
        );
        assert!(commit.contains("Henk-Run: r-exec-1"), "{commit}");
        assert!(commit.contains("Requested-by: github:1234"), "{commit}");
        // The signed-in user who asked is credited by their own id (#399).
        assert!(
            commit.contains("Co-authored-by: user1234 <1234+user1234@users.noreply.github.com>"),
            "{commit}"
        );
        assert_eq!(
            remote_branch_file(remote, "henk/9-export-runs", "src/csv.rs").await,
            "pub fn export() {}\n"
        );
        assert!(f.app.own_pushes.has(report.pull.commit.as_str()));

        let opened = f.hub.opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1);
        assert_eq!(
            (
                opened[0].head.as_str(),
                opened[0].base.as_str(),
                opened[0].title.as_str()
            ),
            ("henk/9-export-runs", "feature", "Export runs")
        );
        let body = &opened[0].body;
        assert!(body.starts_with("Closes #9\n"), "{body}");
        assert!(
            body.contains("JSON export, which the issue does not ask for."),
            "{body}"
        );
        assert!(body.contains("/runs/r-exec-1"), "{body}");
        assert_eq!(report.pull.target.number, 12);
        assert!(f.tracker.comments.lock().unwrap().is_empty());

        let run = record(&f, "r-exec-1").await;
        assert_eq!(run.kind, RunKind::Execute);
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(
            run.summary.as_deref(),
            Some("pull request #12 opened: https://github.com/o/r/pull/12")
        );
        assert_eq!(run.requester.as_deref(), Some("github:1234"));
        let stages = f.app.store.stages(&run.id).await.unwrap();
        let done: Vec<&str> = stages
            .iter()
            .filter(|s| s.state == StageState::Done)
            .map(|s| s.stage.as_str())
            .collect();
        assert_eq!(
            done,
            [
                "requested",
                "started",
                "checkout",
                "session",
                "commit",
                "push",
                "publish",
                "done"
            ]
        );
        let commit_stage = stages
            .iter()
            .find(|s| s.stage.as_str() == "commit" && s.state == StageState::Done)
            .expect("the commit stage is done");
        assert_eq!(commit_stage.detail, "1 file(s) to push");
        assert_eq!(f.provider.live(), 0, "the workspace is closed");

        // Then the pull request is reviewed in a run of its own.
        let review = report.review(Some("github:1234".to_owned()));
        assert_eq!(review.target.number, 12);
        assert_eq!(review.commit.as_ref(), Some(&report.pull.commit));
        assert_eq!(review.trigger, "opened by execute run r-exec-1");
        assert!(review.run.is_none(), "a run of its own");
    }

    #[tokio::test]
    async fn henk_runs_no_checks_of_his_own_and_the_executors_change_is_proposed() {
        let f = fixture("henk-exec-trusted", vec![write_csv(), propose(), done()]).await;
        run_execute(&f.app, request("r-exec-2"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(f.hub.opened.lock().unwrap().len(), 1, "the pull request");
        assert_eq!(record(&f, "r-exec-2").await.status, RunStatus::Finished);
        // The executor runs the repository's checks with bash where it has
        // it; Henk runs none before the push, and CI decides (#396).
        assert!(
            f.provider.ran.lock().unwrap().is_empty(),
            "{:?}",
            f.provider.ran.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn an_executor_that_changes_nothing_or_proposes_nothing_opens_nothing() {
        let f = fixture("henk-exec-none", vec![propose(), done()]).await;
        run_execute(&f.app, request("r-exec-3"), CancellationToken::new())
            .await
            .unwrap_err();
        let comments = f.tracker.comments.lock().unwrap().clone();
        assert!(
            comments[0].contains("the executor changed nothing: JSON export"),
            "{comments:?}"
        );

        // Nudged twice, it still ends without a proposal.
        let f = fixture("henk-exec-mute", vec![write_csv(), done(), done(), done()]).await;
        run_execute(&f.app, request("r-exec-4"), CancellationToken::new())
            .await
            .unwrap_err();
        let comments = f.tracker.comments.lock().unwrap().clone();
        assert!(
            comments[0].contains("the executor ended without proposing a pull request"),
            "{comments:?}"
        );
        assert!(f.hub.opened.lock().unwrap().is_empty());
        assert_eq!(remote_branches(&f.hub.remote).await, ["feature"]);
    }

    /// Pushes the default branch's head to each of `branches`: somebody's
    /// branches, or an earlier run's, by the names a run would take.
    async fn taken(f: &Fixture, name: &str, branches: &[String]) -> String {
        let (head, _) = remote_feature(&f.hub.remote).await;
        let url = f.hub.remote.to_string_lossy().into_owned();
        let other = Checkout::clone_at(
            ScratchDir::new(name).unwrap(),
            &url,
            "feature",
            &CommitSha::parse(&head).unwrap(),
            None,
        )
        .await
        .unwrap();
        for branch in branches {
            other.push(branch).await.unwrap();
        }
        head
    }

    /// A run for an issue that has a branch already (an earlier run's after
    /// a failed push or a closed pull request, or somebody's) takes the
    /// next free name and never pushes over the branch that is there.
    #[tokio::test]
    async fn a_branch_that_exists_is_never_pushed_over_and_a_rerun_takes_the_next_name() {
        let f = fixture("henk-exec-taken", vec![write_csv(), propose(), done()]).await;
        let first = "henk/9-export-runs".to_owned();
        let head = taken(&f, "henk-exec-taken-other", std::slice::from_ref(&first)).await;

        let report = run_execute(&f.app, request("r-exec-5"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report.pull.target.number, 12);
        let opened = f.hub.opened.lock().unwrap().clone();
        assert_eq!(opened[0].head, "henk/9-export-runs-2");
        assert_eq!(
            remote_branch_file(&f.hub.remote, "henk/9-export-runs-2", "src/csv.rs").await,
            "pub fn export() {}\n"
        );
        assert_eq!(
            remote_branch_log(&f.hub.remote, &first, "%H").await.trim(),
            head,
            "untouched"
        );
    }

    /// With every name it would try taken, a run says so on the issue
    /// before the executor starts, and pushes nothing.
    #[tokio::test]
    async fn a_run_with_no_free_branch_name_stops_before_the_session() {
        let f = fixture("henk-exec-all-taken", vec![]).await;
        let names: Vec<String> = (1..=BRANCH_TRIES)
            .map(|n| branch_candidate("henk/9-export-runs", n))
            .collect();
        taken(&f, "henk-exec-all-taken-other", &names).await;

        run_execute(&f.app, request("r-exec-6"), CancellationToken::new())
            .await
            .unwrap_err();
        let comments = f.tracker.comments.lock().unwrap().clone();
        assert!(
            comments[0].contains(
                "I opened no pull request for this issue: the branches henk/9-export-runs to henk/9-export-runs-10 already exist"
            ),
            "{comments:?}"
        );
        assert!(f.hub.opened.lock().unwrap().is_empty());
        let run = record(&f, "r-exec-6").await;
        let stages = f.app.store.stages(&run.id).await.unwrap();
        assert!(
            !stages.iter().any(|s| s.stage.as_str() == "session"),
            "no session: {stages:?}"
        );
    }

    /// The executor is told of `bash` only when it has it: on a backend
    /// that runs model commands, and with `bash` in `[roles.executor]`
    /// tools.
    #[tokio::test]
    async fn the_executor_hears_of_bash_only_when_its_tools_grant_it() {
        let without: Vec<String> = Role::Executor
            .default_tools()
            .into_iter()
            .filter(|tool| tool != "bash")
            .collect();
        for (name, tools, granted) in [
            ("henk-exec-bash", None, true),
            ("henk-exec-nobash", Some(without), false),
        ] {
            let mut f = fixture(name, vec![done()]).await;
            f.app.settings.workspace.default.backend = henk_domain::workspace::BackendKind::Ssh;
            f.app.settings.roles.executor.as_mut().unwrap().tools = tools;
            run_execute(
                &f.app,
                request(&format!("r-{name}")),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
            let requests = f.model.requests();
            let first = requests.first().unwrap();
            let names: Vec<String> = first.tools.iter().map(|t| t.name.to_string()).collect();
            let system = first.system.clone().unwrap_or_default();
            assert_eq!(names.iter().any(|n| n == "bash"), granted, "{names:?}");
            assert_eq!(
                system.contains("commands of your own in the checkout with `bash`"),
                granted,
                "{system}"
            );
        }
    }

    #[tokio::test]
    async fn pull_requests_and_closed_issues_are_not_taken_on() {
        let f = fixture("henk-exec-closed", vec![]).await;
        f.tracker.issue.lock().unwrap().open = false;
        let error = run_execute(&f.app, request("r-exec-6"), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is closed"), "{error}");
        f.tracker.issue.lock().unwrap().is_pull_request = true;
        let error = run_execute(&f.app, request("r-exec-7"), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is a pull request"), "{error}");
        assert!(f.tracker.comments.lock().unwrap().is_empty());
        assert!(
            f.app
                .store
                .run(&RunId::parse("r-exec-7").unwrap())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// [`fixture`] whose run may take `secs` and whose workspaces open on
    /// `pool`, a sandbox host at its limit (#403).
    async fn on_full_host(
        name: &str,
        script: Vec<Completion>,
        secs: u64,
        pool: Arc<crate::workspace::ssh::SshPool>,
    ) -> Fixture {
        let mut f = fixture(name, script).await;
        f.app.settings.execution.as_mut().unwrap().run_timeout_secs = secs;
        f.app.workspace_provider = pool;
        f
    }

    #[tokio::test]
    async fn an_execute_run_whose_time_runs_out_waiting_for_a_sandbox_host_fails_saying_so() {
        let (pool, held) = crate::workspace::ssh::tests::full_host("henk-exec-wait-out").await;
        let f = on_full_host("henk-exec-wait-out-remote", vec![done()], 1, pool).await;
        let error = tokio::time::timeout(
            Duration::from_secs(20),
            run_execute(&f.app, request("r-exec-wait-out"), CancellationToken::new()),
        )
        .await
        .expect("it gives up at its time limit")
        .unwrap_err();
        let why = format!("{error:#}");
        assert!(
            why.contains("the run's time limit was reached while it waited for a sandbox host"),
            "{why}"
        );
        assert!(f.model.requests().is_empty(), "no session");
        let run = record(&f, "r-exec-wait-out").await;
        assert_eq!(run.status, RunStatus::Failed);
        let stages = f.app.store.stages(&run.id).await.unwrap();
        assert!(
            stages.iter().any(|s| s.stage == henk_store::Stage::Checkout
                && s.state == StageState::Failed
                && s.detail.contains("waited for a sandbox host")),
            "{stages:?}"
        );
        assert!(
            !stages.iter().any(|s| s.stage.as_str() == "session"),
            "{stages:?}"
        );
        let comments = f.tracker.comments.lock().unwrap().clone();
        assert!(
            comments
                .iter()
                .any(|c| c.contains("waited for a sandbox host")),
            "{comments:?}"
        );
        held.close().await;
    }

    #[tokio::test]
    async fn the_executor_gets_what_is_left_of_the_run_after_a_wait_for_a_sandbox_host() {
        let (pool, held) = crate::workspace::ssh::tests::full_host("henk-exec-wait").await;
        let mut f = on_full_host("henk-exec-wait-remote", vec![], 4, pool).await;
        // One answer that takes 3s: inside the run's 4s, but not inside
        // what is left of them after a 2s wait for the host.
        let slow = Arc::new(
            ScriptedClient::new("scripted", [Ok(done())]).with_delay(Duration::from_secs(3)),
        );
        f.app
            .models
            .insert("m".to_owned(), Arc::clone(&slow) as Arc<dyn ModelClient>);
        let (result, ()) = tokio::join!(
            run_execute(&f.app, request("r-exec-wait"), CancellationToken::new()),
            async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                held.close().await;
            }
        );
        let why = format!("{:#}", result.unwrap_err());
        assert!(why.contains("the time limit of 4s was reached"), "{why}");
        assert_eq!(slow.requests().len(), 1, "the session began");
        let run = record(&f, "r-exec-wait").await;
        let said: Vec<String> = f
            .app
            .store
            .events(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.message)
            .collect();
        assert!(
            said.iter()
                .any(|m| m.starts_with("got a sandbox host after ")),
            "{said:?}"
        );
        let stages = f.app.store.stages(&run.id).await.unwrap();
        assert!(
            stages
                .iter()
                .any(|s| s.stage == henk_store::Stage::Checkout && s.state == StageState::Done),
            "{stages:?}"
        );
    }
}
