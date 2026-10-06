//! The address run (§3.5): read the open review threads of one pull
//! request, let a model fix what it can in a checkout, push one commit as a
//! fast-forward, then reply in each thread and sum up.
//!
//! Everything that can fail with an error happens before the push. After
//! it, replies and the summary are best-effort: a run that failed never
//! pushed anything, and a run that pushed is never reported as failed.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, StopCause, prompts};
use henk_domain::address::{ThreadOutcome, commit_message, push_refusal};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::run::{RunId, RunKind};
use henk_llm::ChatMessage;
use henk_platform::ReviewTarget;
use henk_platform::address::{AddressWriter, OpenThread};
use henk_session::{SessionSpec, model_id, run_session};
use henk_store::{NewRun, RunStatus};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::address_tools::{AddressContext, AddressState, Settled, address_tools};
use crate::app::App;
use crate::checks::{describe, run_checks};
use crate::git::{Checkout, ScratchDir};
use crate::ids::new_run_id;
use crate::liveness::KeepAlive;

/// A request to address one pull request's review feedback.
#[derive(Debug, Clone)]
pub struct AddressRequest {
    /// The pull request.
    pub target: ReviewTarget,
    /// A note from the requester, if any.
    pub note: Option<String>,
    /// What started it.
    pub trigger: String,
    /// The run id to use, when the caller already announced one.
    pub run: Option<RunId>,
}

/// What an address run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressReport {
    /// The run.
    pub run: RunId,
    /// The pushed commit, when there was a change.
    pub commit: Option<String>,
    /// Threads settled, by outcome.
    pub fixed: usize,
    /// Declined threads.
    pub declined: usize,
    /// Threads answered with a question.
    pub questions: usize,
    /// Threads the model left unsettled.
    pub unsettled: usize,
}

/// Addresses the review feedback on one pull request (§3.5).
///
/// # Errors
///
/// Returns an error when the pull request may not be pushed to (before any
/// run starts), or when the run failed before its push; the failure is then
/// also posted on the pull request.
#[instrument(skip_all, fields(repo = %request.target.repo.path(), number = request.target.number))]
pub async fn run_address(
    app: &App,
    request: AddressRequest,
    cancel: CancellationToken,
) -> anyhow::Result<AddressReport> {
    let config = app
        .settings
        .address
        .as_ref()
        .ok_or_else(|| anyhow!("address runs are not configured"))?;
    if !app.settings.allowlist.allows(&request.target.repo) {
        return Err(anyhow!("{} is not on the allowlist", request.target.repo));
    }
    let writer = app.address_writer(request.target.repo.platform())?;
    let facts = writer
        .pull_facts(&request.target)
        .await
        .context("reading the pull request")?;
    if let Some(why) = push_refusal(&facts.push) {
        return Err(anyhow!(
            "I will not push to #{}: {why}",
            request.target.number
        ));
    }

    let run = request.run.clone().unwrap_or_else(new_run_id);
    let link = app.settings.run_link(&run);
    let requester = format!("discord:{}", config.requester_id);
    app.store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Address,
            platform: request.target.repo.platform(),
            repo: request.target.repo.path(),
            target: request.target.number,
            commit: Some(facts.head.as_str().to_owned()),
            requester: Some(requester.clone()),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await?;
    info!(run = %run, "address run started");
    let _alive = KeepAlive::start(Arc::clone(&app.store), run.clone());
    let model = app.model(&config.model)?;
    let model_id = model_id(model.as_ref());

    let session = Session {
        app,
        writer: Arc::clone(&writer),
        request: &request,
        run: &run,
        link: &link,
        requester: &requester,
        model: Arc::clone(&model),
        model_id: model_id.clone(),
    };
    match session.work(&facts, cancel).await {
        Ok(report) => {
            let summary = summary_line(&report);
            app.store
                .finish_run(&run, RunStatus::Finished, Some(&summary), None)
                .await?;
            info!(run = %run, commit = ?report.commit, "address run ended");
            Ok(report)
        }
        Err(failure) => {
            let reason = format!("{failure:#}");
            error!(reason = %reason, "address run failed");
            let body = marker(&run, &model_id, MarkerKind::Failure).attach(&format!(
                "Addressing the review feedback failed: {reason}. Nothing was pushed. That is my failure, not the pull request's.\n\nRun: {link}"
            ));
            if let Err(post_error) = writer.post_comment(&request.target, &body).await {
                error!(%post_error, "could not post the failure comment");
            }
            app.store
                .finish_run(&run, RunStatus::Failed, None, Some(&reason))
                .await?;
            Err(anyhow!("address run failed: {reason}"))
        }
    }
}

fn marker(run: &RunId, model: &ModelId, kind: MarkerKind) -> Marker {
    Marker {
        run: run.clone(),
        model: model.clone(),
        requested_by: None,
        kind: Some(kind),
        checked_by: None,
        withdrawn: None,
    }
}

fn summary_line(report: &AddressReport) -> String {
    let mut line = format!(
        "{} fixed, {} declined, {} questions",
        report.fixed, report.declined, report.questions
    );
    if report.unsettled > 0 {
        let _ = write!(line, ", {} not settled", report.unsettled);
    }
    match &report.commit {
        Some(commit) => {
            let _ = write!(line, "; pushed {}", commit.get(..12).unwrap_or(commit));
        }
        None => line.push_str("; nothing pushed"),
    }
    line
}

struct Session<'a> {
    app: &'a App,
    writer: Arc<dyn AddressWriter>,
    request: &'a AddressRequest,
    run: &'a RunId,
    link: &'a str,
    requester: &'a str,
    model: Arc<dyn henk_llm::ModelClient>,
    model_id: ModelId,
}

impl Session<'_> {
    /// Everything up to and including the push may fail; the replies after it
    /// do not.
    async fn work(
        &self,
        facts: &henk_platform::address::PullFacts,
        cancel: CancellationToken,
    ) -> anyhow::Result<AddressReport> {
        let Some(config) = self.app.settings.address.as_ref() else {
            return Err(anyhow!("address runs are not configured"));
        };
        let target = &self.request.target;
        let threads = self
            .writer
            .open_threads(target)
            .await
            .context("reading the review threads")?;
        if threads.is_empty() {
            let report = AddressReport {
                run: self.run.clone(),
                commit: None,
                fixed: 0,
                declined: 0,
                questions: 0,
                unsettled: 0,
            };
            self.post_summary(
                &report,
                &[],
                "There are no open review threads; nothing to address.",
            )
            .await;
            return Ok(report);
        }

        let token = self
            .writer
            .git_token()
            .await
            .context("getting a token for git")?;
        let dir = ScratchDir::new(&format!("henk-address-{}", self.run))
            .context("making the checkout directory")?;
        let checkout =
            Checkout::clone_at(dir, &facts.remote, &facts.push.head_ref, &facts.head, token)
                .await
                .context("checking out the pull request")?;

        let context = Arc::new(AddressContext {
            root: checkout.path().to_path_buf(),
            threads: threads.clone(),
            check_commands: config.check_commands.clone(),
            check_timeout: Duration::from_secs(config.check_timeout_secs),
            max_changed_files: config.max_changed_files,
            state: Mutex::new(AddressState::default()),
        });
        self.session(&context, cancel).await?;

        let settled = context
            .state
            .lock()
            .map(|s| s.settled.clone())
            .map_err(|_| anyhow!("address state unavailable"))?;
        let changed = checkout
            .changed_files()
            .await
            .context("listing the changes")?;
        if changed.len() > config.max_changed_files {
            return Err(anyhow!(
                "{} files changed; a run may change at most {}",
                changed.len(),
                config.max_changed_files
            ));
        }

        let (commit, checks_text) = if changed.is_empty() {
            (None, String::new())
        } else {
            self.commit_and_push(&checkout, facts, &threads, &settled, changed.len())
                .await?
        };

        Ok(self
            .reply_all(&threads, &settled, commit, &checks_text)
            .await)
    }

    /// Runs the checks once more, commits as Henk, makes sure the branch has
    /// not moved, and pushes. Returns the commit and the checks' report.
    async fn commit_and_push(
        &self,
        checkout: &Checkout,
        facts: &henk_platform::address::PullFacts,
        threads: &[OpenThread],
        settled: &std::collections::BTreeMap<String, Settled>,
        files: usize,
    ) -> anyhow::Result<(Option<String>, String)> {
        let Some(config) = self.app.settings.address.as_ref() else {
            return Err(anyhow!("address runs are not configured"));
        };
        let target = &self.request.target;
        let results = run_checks(
            checkout.path(),
            &config.check_commands,
            Duration::from_secs(config.check_timeout_secs),
        )
        .await;
        let checks_text = if results.is_empty() {
            String::new()
        } else {
            describe(&results)
        };
        let fixed_notes: Vec<String> = threads
            .iter()
            .filter_map(|t| {
                let s = settled.get(&t.thread_id)?;
                (s.outcome == ThreadOutcome::Fixed).then(|| match (&t.path, t.line) {
                    (Some(path), Some(line)) => format!("{path}:{line}: {}", s.reply),
                    (Some(path), None) => format!("{path}: {}", s.reply),
                    _ => s.reply.clone(),
                })
            })
            .collect();
        let message = commit_message(&fixed_notes, self.run, Some(self.requester));
        let identity = self
            .writer
            .commit_identity()
            .await
            .context("reading Henk's commit identity")?;
        // Read again just before pushing: a head that moved means someone
        // else pushed, and their work is not overwritten.
        let now = self
            .writer
            .pull_facts(target)
            .await
            .context("reading the pull request again")?;
        if now.head != facts.head {
            return Err(anyhow!(
                "the branch moved to {} while I worked",
                now.head.short()
            ));
        }
        if let Some(why) = push_refusal(&now.push) {
            return Err(anyhow!("I may no longer push: {why}"));
        }
        let sha = checkout
            .commit(&identity, &message)
            .await
            .context("committing")?;
        checkout
            .push(&facts.push.head_ref)
            .await
            .context("pushing")?;
        info!(commit = %sha, files, "pushed");
        Ok((Some(sha), checks_text))
    }

    async fn session(
        &self,
        context: &Arc<AddressContext>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let Some(config) = self.app.settings.address.as_ref() else {
            return Err(anyhow!("address runs are not configured"));
        };
        let target = &self.request.target;
        let note = self
            .request
            .note
            .as_deref()
            .map_or(String::new(), |n| format!("Their note: {n}"));
        let reference = format!("#{}", target.number);
        let system = format!(
            "{}\n\n{}",
            prompts::PERSONA,
            prompts::render(
                prompts::ADDRESS,
                &[
                    ("ref", &reference),
                    ("repo", &target.repo.path()),
                    ("note", &note),
                    ("max_files", &config.max_changed_files.to_string()),
                ],
            )
        );
        let opening = ChatMessage::user(format!(
            "The open review threads on {reference}, as data:\n\n{}",
            context.threads_text()
        ));
        let spec = SessionSpec {
            name: "address".to_owned(),
            model: Arc::clone(&self.model),
            system,
            opening: vec![opening],
            tools: address_tools(context),
            limits: AgentConfig {
                max_turns: config.max_turns,
                timeout: Duration::from_secs(config.timeout_secs),
                ..AgentConfig::default()
            },
            continuation: None,
            turn_warning: None,
        };
        let outcome = run_session(self.app.store.as_ref(), self.run, spec, cancel).await;
        session_result(outcome.stop, config.timeout_secs)
    }

    /// Replies in every settled thread and posts the summary. Best-effort:
    /// the push already happened.
    async fn reply_all(
        &self,
        threads: &[OpenThread],
        settled: &std::collections::BTreeMap<String, Settled>,
        commit: Option<String>,
        checks_text: &str,
    ) -> AddressReport {
        let target = &self.request.target;
        let mut report = AddressReport {
            run: self.run.clone(),
            commit: commit.clone(),
            fixed: 0,
            declined: 0,
            questions: 0,
            unsettled: 0,
        };
        let mut problems = Vec::new();
        for thread in threads {
            let Some(decision) = settled.get(&thread.thread_id) else {
                report.unsettled += 1;
                continue;
            };
            let text = match (decision.outcome, &commit) {
                (ThreadOutcome::Fixed, Some(sha)) => {
                    report.fixed += 1;
                    format!(
                        "Fixed in {}: {}",
                        self.writer.commit_url(target, sha),
                        decision.reply
                    )
                }
                (ThreadOutcome::Fixed, None) => {
                    // A fix with no change is no fix: say nothing in the thread.
                    report.unsettled += 1;
                    problems.push(format!(
                        "a thread was marked fixed without any change ({})",
                        thread.thread_id
                    ));
                    continue;
                }
                (ThreadOutcome::Declined, _) => {
                    report.declined += 1;
                    decision.reply.clone()
                }
                (ThreadOutcome::Question, _) => {
                    report.questions += 1;
                    decision.reply.clone()
                }
            };
            let body = marker(self.run, &self.model_id, MarkerKind::Reply).attach(&text);
            if let Err(error) = self.writer.reply_in_thread(target, thread, &body).await {
                warn!(%error, thread = %thread.thread_id, "could not reply");
                problems.push(format!("a reply could not be posted ({error})"));
                continue;
            }
            if decision.outcome == ThreadOutcome::Fixed
                && thread.started_by_henk()
                && let Err(error) = self.writer.resolve_thread(target, &thread.thread_id).await
            {
                warn!(%error, thread = %thread.thread_id, "could not resolve");
                problems.push(format!("a thread could not be resolved ({error})"));
            }
        }
        let mut headline = match &commit {
            Some(sha) => format!(
                "Addressed the review feedback in {}.",
                self.writer.commit_url(target, sha)
            ),
            None => "Addressed the review feedback without changing the code.".to_owned(),
        };
        if !checks_text.is_empty() {
            let _ = write!(
                headline,
                "\n\nChecks after my change:\n\n```\n{checks_text}\n```"
            );
        }
        self.post_summary(&report, &problems, &headline).await;
        report
    }

    async fn post_summary(&self, report: &AddressReport, problems: &[String], headline: &str) {
        let mut text = format!("{headline}\n\n{}.", summary_line(report));
        for problem in problems {
            let _ = write!(text, "\n- {problem}");
        }
        let _ = write!(text, "\n\nRun: {}", self.link);
        let body = marker(self.run, &self.model_id, MarkerKind::Reply).attach(&text);
        if let Err(error) = self.writer.post_comment(&self.request.target, &body).await {
            warn!(%error, "could not post the summary");
        }
    }
}

/// How the address session ended, as the run's result. A model that
/// declined fails the run; it did not finish.
fn session_result(stop: StopCause, timeout_secs: u64) -> anyhow::Result<()> {
    match stop {
        StopCause::EndTurn | StopCause::MaxTurns => Ok(()),
        StopCause::Timeout => Err(anyhow!("the time limit of {timeout_secs}s was reached")),
        StopCause::Cancelled => Err(anyhow!("cancelled")),
        StopCause::ModelError(error) => Err(anyhow!("model error: {error}")),
        StopCause::Refused(why) => Err(anyhow!("the model declined to address ({why})")),
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use henk_domain::address::PushFacts;
    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::review::CommitSha;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{
        Block, ChatMessage, Completion, ModelClient, Role, StopReason, ToolArguments, ToolCall,
        Usage,
    };
    use henk_platform::address::{CommitIdentity, PullFacts, ThreadNote};
    use henk_platform::{PlatformError, PostedComment};
    use secrecy::SecretString;
    use serde_json::{Value, json};

    use super::*;
    use crate::config::Config;
    use crate::git::tests::{bare_remote, remote_feature};

    const CONFIG: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["o"]
[models.m]
provider = "open_ai"
base_url = "https://x.test/v1"
api_key_env = "UNUSED"
model = "scripted"
[address]
model = "m"
requester_id = 3
check_commands = [["true"]]
"#;

    /// A GitHub as far as an address run sees it, over a local bare remote.
    struct FakeHub {
        facts: PullFacts,
        /// What the second read of the pull request returns, when it differs.
        later: Option<PullFacts>,
        reads: AtomicUsize,
        threads: Vec<OpenThread>,
        replies: Mutex<Vec<(String, String)>>,
        resolved: Mutex<Vec<String>>,
        comments: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl AddressWriter for FakeHub {
        async fn pull_facts(&self, _: &ReviewTarget) -> Result<PullFacts, PlatformError> {
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(match (&self.later, read) {
                (Some(later), 1..) => later.clone(),
                _ => self.facts.clone(),
            })
        }
        async fn open_threads(&self, _: &ReviewTarget) -> Result<Vec<OpenThread>, PlatformError> {
            Ok(self.threads.clone())
        }
        async fn git_token(&self) -> Result<Option<SecretString>, PlatformError> {
            Ok(None)
        }
        async fn commit_identity(&self) -> Result<CommitIdentity, PlatformError> {
            Ok(CommitIdentity {
                name: "meneer-henk[bot]".to_owned(),
                email: "1+meneer-henk[bot]@users.noreply.github.com".to_owned(),
            })
        }
        fn commit_url(&self, target: &ReviewTarget, sha: &str) -> String {
            format!("https://github.example/{}/commit/{sha}", target.repo.path())
        }
        async fn reply_in_thread(
            &self,
            _: &ReviewTarget,
            thread: &OpenThread,
            body: &str,
        ) -> Result<PostedComment, PlatformError> {
            self.replies
                .lock()
                .unwrap()
                .push((thread.thread_id.clone(), body.to_owned()));
            Ok(PostedComment {
                id: "r".to_owned(),
                node_id: None,
                url: String::new(),
            })
        }
        async fn resolve_thread(&self, _: &ReviewTarget, id: &str) -> Result<(), PlatformError> {
            self.resolved.lock().unwrap().push(id.to_owned());
            Ok(())
        }
        async fn post_comment(
            &self,
            _: &ReviewTarget,
            body: &str,
        ) -> Result<PostedComment, PlatformError> {
            self.comments.lock().unwrap().push(body.to_owned());
            Ok(PostedComment {
                id: "c".to_owned(),
                node_id: None,
                url: String::new(),
            })
        }
    }

    fn thread(id: &str, by_henk: bool) -> OpenThread {
        OpenThread {
            thread_id: id.to_owned(),
            path: Some("src/a.rs".to_owned()),
            line: Some(2),
            outdated: false,
            notes: vec![ThreadNote {
                comment_id: format!("{id}-1"),
                author: if by_henk { "meneer-henk[bot]" } else { "alice" }.to_owned(),
                author_id: Some(7),
                by_henk,
                body: "x should be 2.".to_owned(),
            }],
        }
    }

    fn facts(remote: &std::path::Path, head: &CommitSha) -> PullFacts {
        PullFacts {
            push: PushFacts {
                open: true,
                head_repo: Some("o/r".to_owned()),
                base_repo: "o/r".to_owned(),
                head_ref: "feature".to_owned(),
                default_branch: "main".to_owned(),
                head_protected: false,
            },
            head: head.clone(),
            remote: remote.to_string_lossy().into_owned(),
        }
    }

    fn call(name: &str, arguments: Value) -> Completion {
        Completion {
            message: ChatMessage {
                role: Role::Assistant,
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
            message: ChatMessage::assistant("All threads settled."),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn app(hub: Arc<FakeHub>, script: Vec<Completion>) -> App {
        let settings = Config::parse(CONFIG)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let model = ScriptedClient::new("scripted", script.into_iter().map(Ok));
        let mut models: BTreeMap<String, Arc<dyn ModelClient>> = BTreeMap::new();
        models.insert("m".to_owned(), Arc::new(model));
        App {
            settings,
            store: Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
            models,
            github: None,
            gitlab: None,
            shutdown: CancellationToken::new(),
            test_writer: None,
            test_session: None,
            test_address_writer: Some(hub as Arc<dyn AddressWriter>),
            test_issue_writer: None,
        }
    }

    fn request(run: &str) -> AddressRequest {
        AddressRequest {
            target: ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 7,
            },
            note: None,
            trigger: "test".to_owned(),
            run: Some(RunId::parse(run).unwrap()),
        }
    }

    fn hub(remote: &std::path::Path, head: &CommitSha, later: Option<PullFacts>) -> Arc<FakeHub> {
        Arc::new(FakeHub {
            facts: facts(remote, head),
            later,
            reads: AtomicUsize::new(0),
            threads: vec![thread("T1", true), thread("T2", false)],
            replies: Mutex::default(),
            resolved: Mutex::default(),
            comments: Mutex::default(),
        })
    }

    #[tokio::test]
    async fn a_fix_is_pushed_as_one_commit_and_every_thread_hears_how_it_ended() {
        let (remote, head) = bare_remote("henk-address-fix").await;
        let hub = hub(remote.path(), &head, None);
        let app = app(
            Arc::clone(&hub),
            vec![
                call(
                    "edit_file",
                    json!({"path": "src/a.rs", "old": "let x = 1;", "new": "let x = 2;"}),
                ),
                call(
                    "settle_thread",
                    json!({"thread_id": "T1", "outcome": "fixed", "reply": "Set x to 2."}),
                ),
                call(
                    "settle_thread",
                    json!({"thread_id": "T2", "outcome": "declined", "reply": "One value is enough here."}),
                ),
                done(),
            ],
        );
        let report = run_address(&app, request("r-addr-1"), CancellationToken::new())
            .await
            .unwrap();
        let sha = report.commit.clone().unwrap();
        assert_eq!((report.fixed, report.declined, report.unsettled), (1, 1, 0));

        let (remote_head, log) = remote_feature(remote.path()).await;
        assert_eq!(remote_head, sha, "pushed");
        assert!(
            log.starts_with("meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>"),
            "{log}"
        );
        assert!(log.contains("- src/a.rs:2: Set x to 2."), "{log}");
        assert!(
            log.contains("Henk-Run: r-addr-1\nRequested-by: discord:3"),
            "{log}"
        );

        let replies = hub.replies.lock().unwrap().clone();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].0, "T1");
        assert!(
            replies[0].1.starts_with(&format!(
                "Fixed in https://github.example/o/r/commit/{sha}: Set x to 2."
            )),
            "{}",
            replies[0].1
        );
        assert!(replies[0].1.contains("kind=reply"), "{}", replies[0].1);
        assert!(replies[1].1.starts_with("One value is enough here."));
        assert_eq!(
            *hub.resolved.lock().unwrap(),
            ["T1"],
            "only Henk's own thread is resolved"
        );

        let comments = hub.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1);
        assert!(
            comments[0].contains("1 fixed, 1 declined, 0 questions"),
            "{}",
            comments[0]
        );
        assert!(comments[0].contains("$ true: passed"), "{}", comments[0]);
        assert!(comments[0].contains("/runs/r-addr-1"), "{}", comments[0]);
        let record = app
            .store
            .run(&RunId::parse("r-addr-1").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RunStatus::Finished);
        assert_eq!(record.kind, RunKind::Address);
    }

    #[tokio::test]
    async fn without_a_change_nothing_is_pushed_and_a_claimed_fix_is_not_believed() {
        let (remote, head) = bare_remote("henk-address-nochange").await;
        let hub = hub(remote.path(), &head, None);
        let app = app(
            Arc::clone(&hub),
            vec![
                call(
                    "settle_thread",
                    json!({"thread_id": "T1", "outcome": "fixed", "reply": "Done."}),
                ),
                call(
                    "settle_thread",
                    json!({"thread_id": "T2", "outcome": "question", "reply": "Should x be 2 in tests too?"}),
                ),
                done(),
            ],
        );
        let report = run_address(&app, request("r-addr-2"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report.commit, None);
        assert_eq!(
            (report.fixed, report.questions, report.unsettled),
            (0, 1, 1)
        );
        assert_eq!(
            remote_feature(remote.path()).await.0,
            head.as_str(),
            "nothing pushed"
        );
        let replies = hub.replies.lock().unwrap().clone();
        assert_eq!(replies.len(), 1, "no reply for a fix that changed nothing");
        assert_eq!(replies[0].0, "T2");
        assert!(hub.resolved.lock().unwrap().is_empty());
        let summary = hub.comments.lock().unwrap()[0].clone();
        assert!(
            summary.contains("marked fixed without any change"),
            "{summary}"
        );
        assert!(summary.contains("nothing pushed"), "{summary}");
    }

    #[tokio::test]
    async fn a_branch_that_moved_while_henk_worked_gets_nothing_pushed() {
        let (remote, head) = bare_remote("henk-address-moved").await;
        let mut later = facts(remote.path(), &head);
        later.head = CommitSha::parse("1111111111111111111111111111111111111111").unwrap();
        let hub = hub(remote.path(), &head, Some(later));
        let app = app(
            Arc::clone(&hub),
            vec![
                call(
                    "edit_file",
                    json!({"path": "src/a.rs", "old": "let x = 1;", "new": "let x = 2;"}),
                ),
                call(
                    "settle_thread",
                    json!({"thread_id": "T1", "outcome": "fixed", "reply": "Set x to 2."}),
                ),
                done(),
            ],
        );
        let error = run_address(&app, request("r-addr-3"), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("the branch moved"), "{error}");
        assert_eq!(
            remote_feature(remote.path()).await.0,
            head.as_str(),
            "nothing pushed"
        );
        assert!(
            hub.replies.lock().unwrap().is_empty(),
            "no reply speaks of a fix that is not there"
        );
        let comments = hub.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1);
        assert!(
            comments[0].contains("Nothing was pushed"),
            "{}",
            comments[0]
        );
        assert!(comments[0].contains("kind=failure"), "{}", comments[0]);
        let record = app
            .store
            .run(&RunId::parse("r-addr-3").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn a_fork_or_protected_branch_is_refused_before_anything_runs() {
        let (remote, head) = bare_remote("henk-address-refused").await;
        for (case, change) in [
            (
                "fork",
                PushFacts {
                    head_repo: Some("fork/r".to_owned()),
                    ..facts(remote.path(), &head).push
                },
            ),
            (
                "protected",
                PushFacts {
                    head_protected: true,
                    ..facts(remote.path(), &head).push
                },
            ),
            (
                "closed",
                PushFacts {
                    open: false,
                    ..facts(remote.path(), &head).push
                },
            ),
        ] {
            let hub = Arc::new(FakeHub {
                facts: PullFacts {
                    push: change,
                    ..facts(remote.path(), &head)
                },
                ..Arc::into_inner(hub(remote.path(), &head, None)).unwrap()
            });
            let app = app(Arc::clone(&hub), vec![]);
            let error = run_address(&app, request("r-addr-4"), CancellationToken::new())
                .await
                .unwrap_err();
            assert!(
                error.to_string().starts_with("I will not push"),
                "{case}: {error}"
            );
            assert!(hub.comments.lock().unwrap().is_empty(), "{case}");
            assert!(
                app.store
                    .run(&RunId::parse("r-addr-4").unwrap())
                    .await
                    .unwrap()
                    .is_none(),
                "{case}: no run"
            );
        }
    }

    #[test]
    fn a_refused_address_session_fails_and_says_why() {
        let error = session_result(StopCause::Refused("refusal".to_owned()), 60).unwrap_err();
        assert_eq!(error.to_string(), "the model declined to address (refusal)");
        assert!(session_result(StopCause::EndTurn, 60).is_ok());
    }
}
