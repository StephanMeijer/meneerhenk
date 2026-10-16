//! Rebases Henk's own pull requests when their base branch moves (§3.6,
//! #579): a push to a repository's default branch, for a repository that
//! has `rebase_when_behind` on.

use std::sync::Arc;

use henk_domain::queue::Decision;
use henk_domain::run::{RunId, RunKind};
use henk_events::{Event, EventKind, Handled, Listener};
use henk_platform::ReviewTarget;
use henk_platform::push::BehindPull;
use henk_store::{Page, RunFilter, RunStatus};
use tracing::{info, instrument, warn};

use crate::coordinator::Coordinator;

/// The most pull requests one push starts a rebase of. The slot queue
/// bounds how many run at once; this bounds how many wait. The rest wait
/// for the next move of the base, or a comment.
const MOST_PER_PUSH: usize = 20;

/// How many of a pull request's latest rebase runs are looked at for a
/// failed attempt at its head.
const LOOKED_AT: u32 = 10;

/// Turns a push to a default branch into background rebases of the pull
/// requests of Henk's own that it left behind.
pub struct RebaseOnMoveListener {
    coordinator: Arc<Coordinator>,
}

impl RebaseOnMoveListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }

    /// Whether a rebase Henk started himself already failed on this pull
    /// request while its branch was at `pull.head`: trying again at every
    /// push to the base would say the same thing every time. A new commit
    /// on the branch, or a person's request, lifts it. Only the runs he
    /// started himself record the head (as the run's commit).
    async fn failed_at_this_head(&self, target: &ReviewTarget, pull: &BehindPull) -> bool {
        let filter = RunFilter {
            kind: Some(RunKind::Rebase),
            platform: Some(target.repo.platform()),
            repo: Some(target.repo.path()),
            target: Some(target.number),
            ..RunFilter::default()
        };
        match self
            .coordinator
            .app()
            .store
            .list_runs(&filter, Page::new(LOOKED_AT, 0))
            .await
        {
            Ok(runs) => runs.iter().any(|run| {
                run.status == RunStatus::Failed && run.commit.as_deref() == Some(pull.head.as_str())
            }),
            Err(error) => {
                warn!(%error, "could not look for an earlier rebase; trying");
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl Listener for RebaseOnMoveListener {
    fn name(&self) -> &'static str {
        "rebase_on_move"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let EventKind::BranchPushed {
            repo, branch, head, ..
        } = &event.kind
        else {
            return Handled::Ignored("not a push to a base branch".to_owned());
        };
        let app = self.coordinator.app();
        let settings = &app.settings;
        if !settings.allowlist.allows(repo) {
            return Handled::Ignored(format!("{repo} is not on the allowlist"));
        }
        let (Some(executor), Some(execution)) = (&settings.roles.executor, &settings.execution)
        else {
            return Handled::Ignored("rebasing needs [roles.executor] and [execution]".to_owned());
        };
        if !executor.rebases_when_behind(repo) {
            return Handled::Ignored(format!("{repo} is not rebased when its base moves"));
        }
        let behind = match app.push_writer(repo) {
            Ok(writer) => {
                match writer
                    .open_pulls_behind(repo, branch, &execution.branch_prefix)
                    .await
                {
                    Ok(behind) => behind,
                    Err(error) => {
                        return Handled::Failed(format!(
                            "listing the pull requests behind {branch}: {error}"
                        ));
                    }
                }
            }
            Err(error) => return Handled::Failed(format!("{error:#}")),
        };
        if behind.is_empty() {
            return Handled::Ignored(format!("none of Henk's pull requests is behind {branch}"));
        }
        if behind.len() > MOST_PER_PUSH {
            warn!(
                repo = %repo, branch, behind = behind.len(),
                "more pull requests behind than one push rebases; the rest wait for the next"
            );
        }

        let trigger = format!(
            "{}: {branch} is at {}",
            crate::rebase::MOVED_TRIGGER,
            head.short()
        );
        let (mut started, mut joined, mut guarded) = (None, None, 0_usize);
        for pull in behind.iter().take(MOST_PER_PUSH) {
            let target = ReviewTarget {
                repo: repo.clone(),
                number: pull.number,
            };
            if self.failed_at_this_head(&target, pull).await {
                guarded += 1;
                continue;
            }
            let (decision, run) = self.coordinator.submit_rebase_automatic(
                target,
                trigger.clone(),
                pull.head.clone(),
            );
            info!(repo = %repo, pull = pull.number, %run, ?decision, behind_by = pull.behind_by, "rebasing a pull request that fell behind");
            remember(decision, run, &mut started, &mut joined);
        }
        match (started, joined) {
            (Some(run), _) => Handled::Started(run),
            (None, Some(run)) => Handled::Joined(run),
            (None, None) => Handled::Ignored(format!(
                "{guarded} pull request(s) behind {branch} failed to rebase at their present head; left to a person"
            )),
        }
    }
}

/// Notes a run the listener started or joined: the first of each is what the
/// event reports.
fn remember(
    decision: Decision,
    run: RunId,
    started: &mut Option<RunId>,
    joined: &mut Option<RunId>,
) {
    let slot = if decision == Decision::Join {
        joined
    } else {
        started
    };
    slot.get_or_insert(run);
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
    use std::sync::Mutex;
    use std::time::Duration;

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::review::CommitSha;
    use henk_events::{EventSource, Sender};
    use henk_llm::ModelClient;
    use henk_llm::testing::ScriptedClient;
    use henk_platform::PlatformError;
    use henk_platform::PlatformWriter;
    use henk_platform::push::{
        CommitIdentity, GitCredential, NewPull, OpenedPull, PullFacts, PushWriter, RepoHead,
    };
    use henk_store::{NewRun, RunRecord, RunStatus};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::app::App;
    use crate::config::Config;
    use crate::listeners::testing::FakeWriter;

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
[roles.executor.repositories."o/r"]
rebase_when_behind = true
"#;

    fn head(n: u8) -> CommitSha {
        CommitSha::parse(&format!("{n:02x}").repeat(20)).unwrap()
    }

    fn pull(number: u64, n: u8) -> BehindPull {
        BehindPull {
            number,
            head_ref: format!("henk/{number}-x"),
            head: head(n),
            behind_by: 2,
        }
    }

    /// The platform as the listener sees it: it lists `behind` for any base,
    /// or fails, and notes what it was asked.
    struct Pulls {
        behind: Vec<BehindPull>,
        fails: bool,
        asked: Mutex<Vec<(String, String, String)>>,
    }

    fn unused() -> PlatformError {
        PlatformError::Decode("not used by the listener".to_owned())
    }

    #[async_trait::async_trait]
    impl PushWriter for Pulls {
        async fn pull_facts(&self, _: &ReviewTarget) -> Result<PullFacts, PlatformError> {
            Err(unused())
        }
        async fn repo_head(&self, _: &RepoRef) -> Result<RepoHead, PlatformError> {
            Err(unused())
        }
        async fn git_credential(&self) -> Result<Option<GitCredential>, PlatformError> {
            Ok(None)
        }
        async fn commit_identity(&self) -> Result<CommitIdentity, PlatformError> {
            Err(unused())
        }
        async fn user_login(&self, _: u64) -> Result<String, PlatformError> {
            Err(unused())
        }
        fn noreply_host(&self) -> Result<String, PlatformError> {
            Ok("github.com".to_owned())
        }
        async fn open_pull_request(
            &self,
            _: &RepoRef,
            _: &NewPull,
        ) -> Result<OpenedPull, PlatformError> {
            Err(unused())
        }
        async fn open_pulls_behind(
            &self,
            repo: &RepoRef,
            base: &str,
            prefix: &str,
        ) -> Result<Vec<BehindPull>, PlatformError> {
            self.asked
                .lock()
                .unwrap()
                .push((repo.path(), base.to_owned(), prefix.to_owned()));
            if self.fails {
                return Err(PlatformError::Decode("down".to_owned()));
            }
            Ok(self.behind.clone())
        }
    }

    struct Fixture {
        listener: RebaseOnMoveListener,
        app: Arc<App>,
        pulls: Arc<Pulls>,
    }

    fn fixture(config: &str, behind: Vec<BehindPull>, fails: bool) -> Fixture {
        let settings = Config::parse(config)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut models: BTreeMap<String, Arc<dyn ModelClient>> = BTreeMap::new();
        models.insert(
            "m".to_owned(),
            Arc::new(ScriptedClient::new("scripted", [])) as Arc<dyn ModelClient>,
        );
        let pulls = Arc::new(Pulls {
            behind,
            fails,
            asked: Mutex::default(),
        });
        let app = Arc::new(App {
            settings,
            store: Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
            models,
            github: None,
            gitlab: None,
            shutdown: CancellationToken::new(),
            live_runs: crate::liveness::LiveRuns::default(),
            feed: crate::live::Feed::default(),
            cancels: crate::cancel::Cancels::default(),
            workspace_provider: Arc::new(crate::workspace::fake::FakeProvider::default()),
            own_pushes: crate::push::OwnPushes::default(),
            test_writer: Some(Arc::new(FakeWriter::default()) as Arc<dyn PlatformWriter>),
            test_session: None,
            test_push_writer: Some(Arc::clone(&pulls) as Arc<dyn PushWriter>),
            test_issue_writer: None,
            test_ci_reader: None,
        });
        Fixture {
            listener: RebaseOnMoveListener::new(Arc::new(Coordinator::new(Arc::clone(&app)))),
            app,
            pulls,
        }
    }

    fn push(repo: &str, branch: &str) -> Arc<Event> {
        Arc::new(Event {
            id: henk_domain::run::EventId::parse("e-1").unwrap(),
            received_at: "2026-10-16T00:00:00Z".into(),
            source: EventSource::GitHubWebhook {
                delivery: "d1".to_owned(),
            },
            kind: EventKind::BranchPushed {
                repo: RepoRef::parse(Platform::GitHub, repo).unwrap(),
                branch: branch.to_owned(),
                head: head(0xaa),
                sender: Sender {
                    login: "alice".to_owned(),
                    is_bot: false,
                },
            },
            payload: None,
        })
    }

    async fn rebases(app: &App) -> Vec<RunRecord> {
        let filter = RunFilter {
            kind: Some(RunKind::Rebase),
            ..RunFilter::default()
        };
        app.store
            .list_runs(&filter, Page::new(100, 0))
            .await
            .unwrap()
    }

    /// Waits until `count` rebase runs are recorded.
    async fn rebases_recorded(app: &App, count: usize) -> Vec<RunRecord> {
        for _ in 0..200 {
            let runs = rebases(app).await;
            if runs.len() >= count {
                return runs;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("fewer than {count} rebase runs were recorded");
    }

    /// A rebase run of Henk's own on pull request `number` that ended as
    /// `status`, having seen the branch at `seen`.
    async fn earlier(
        app: &App,
        run: &str,
        number: u64,
        seen: Option<CommitSha>,
        status: RunStatus,
    ) {
        let id = RunId::parse(run).unwrap();
        app.store
            .create_run(&NewRun {
                id: id.clone(),
                kind: RunKind::Rebase,
                platform: Platform::GitHub,
                repo: "o/r".to_owned(),
                target: number,
                commit: seen.map(|sha| sha.as_str().to_owned()),
                requester: None,
                trigger: "earlier".to_owned(),
                link: String::new(),
            })
            .await
            .unwrap();
        app.store
            .finish_run(&id, status, None, Some("it did not work"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_push_to_the_base_starts_a_rebase_of_each_of_henks_pull_requests_behind_it() {
        let f = fixture(CONFIG, vec![pull(11, 0x11), pull(12, 0x12)], false);
        let handled = f.listener.handle(push("o/r", "main")).await;
        assert!(matches!(handled, Handled::Started(_)), "{handled:?}");
        assert_eq!(
            *f.pulls.asked.lock().unwrap(),
            [("o/r".to_owned(), "main".to_owned(), "henk/".to_owned())],
            "asked for the base that moved, with the prefix of Henk's branches"
        );
        let runs = rebases_recorded(&f.app, 2).await;
        let mut targets: Vec<(u64, Option<String>)> = runs
            .iter()
            .map(|run| (run.target, run.commit.clone()))
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            [
                (11, Some(head(0x11).as_str().to_owned())),
                (12, Some(head(0x12).as_str().to_owned()))
            ],
            "each records the head it saw"
        );
        for run in &runs {
            assert!(
                run.trigger.starts_with(crate::rebase::MOVED_TRIGGER),
                "{}",
                run.trigger
            );
            assert!(
                run.trigger.contains("main is at aaaaaaa"),
                "{}",
                run.trigger
            );
            assert_eq!(run.requester, None, "nobody asked");
        }
    }

    #[tokio::test]
    async fn nothing_starts_where_the_repository_is_not_set_to_it_or_nothing_is_behind() {
        // Not set for this repository.
        let off = fixture(&CONFIG.replace("true", "false"), vec![pull(11, 1)], false);
        let handled = off.listener.handle(push("o/r", "main")).await;
        assert!(
            matches!(&handled, Handled::Ignored(why) if why.contains("not rebased when its base moves")),
            "{handled:?}"
        );
        assert!(
            off.pulls.asked.lock().unwrap().is_empty(),
            "the platform is not asked"
        );

        // Not on the allowlist.
        let f = fixture(CONFIG, vec![pull(11, 1)], false);
        let handled = f.listener.handle(push("elsewhere/r", "main")).await;
        assert!(
            matches!(&handled, Handled::Ignored(why) if why.contains("allowlist")),
            "{handled:?}"
        );

        // Not a push event at all.
        let mut event = (*push("o/r", "main")).clone();
        event.kind = EventKind::Ignored("push: feature is not the default branch".to_owned());
        let handled = f.listener.handle(Arc::new(event)).await;
        assert!(
            matches!(&handled, Handled::Ignored(why) if why.contains("not a push")),
            "{handled:?}"
        );

        // Nothing of Henk's is behind.
        let none = fixture(CONFIG, vec![], false);
        let handled = none.listener.handle(push("o/r", "main")).await;
        assert!(
            matches!(&handled, Handled::Ignored(why) if why.contains("none of Henk's pull requests is behind main")),
            "{handled:?}"
        );
        assert!(rebases(&none.app).await.is_empty());

        // The platform cannot say.
        let down = fixture(CONFIG, vec![], true);
        let handled = down.listener.handle(push("o/r", "main")).await;
        assert!(
            matches!(&handled, Handled::Failed(why) if why.contains("listing")),
            "{handled:?}"
        );
    }

    #[tokio::test]
    async fn a_pull_request_he_failed_to_rebase_at_this_head_is_left_to_a_person_until_it_changes()
    {
        let f = fixture(CONFIG, vec![pull(11, 0x11), pull(12, 0x12)], false);
        // #11 failed by itself at the head it has now; #12 failed at an older
        // head.
        earlier(
            &f.app,
            "r-earlier-11",
            11,
            Some(head(0x11)),
            RunStatus::Failed,
        )
        .await;
        earlier(
            &f.app,
            "r-earlier-12",
            12,
            Some(head(0x01)),
            RunStatus::Failed,
        )
        .await;
        let handled = f.listener.handle(push("o/r", "main")).await;
        assert!(matches!(handled, Handled::Started(_)), "{handled:?}");
        let runs = rebases_recorded(&f.app, 3).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let runs_now = rebases(&f.app).await;
        assert_eq!(
            runs_now.len(),
            runs.len().max(3),
            "only #12 started a new run"
        );
        let started: Vec<u64> = runs_now
            .iter()
            .filter(|run| run.id.as_str() != "r-earlier-11" && run.id.as_str() != "r-earlier-12")
            .map(|run| run.target)
            .collect();
        assert_eq!(started, [12]);

        // With only the guarded one behind, there is nothing to start.
        let alone = fixture(CONFIG, vec![pull(11, 0x11)], false);
        earlier(
            &alone.app,
            "r-earlier-a",
            11,
            Some(head(0x11)),
            RunStatus::Failed,
        )
        .await;
        let handled = alone.listener.handle(push("o/r", "main")).await;
        assert!(
            matches!(&handled, Handled::Ignored(why) if why.contains("left to a person")),
            "{handled:?}"
        );
        assert_eq!(rebases(&alone.app).await.len(), 1, "no new run");
    }

    #[tokio::test]
    async fn a_success_a_manual_run_or_an_unrecorded_head_never_holds_a_rebase_back() {
        for (case, seen, status) in [
            (
                "finished at this head",
                Some(head(0x11)),
                RunStatus::Finished,
            ),
            ("failed, but asked for by a person", None, RunStatus::Failed),
        ] {
            let f = fixture(CONFIG, vec![pull(11, 0x11)], false);
            earlier(&f.app, "r-earlier", 11, seen, status).await;
            let handled = f.listener.handle(push("o/r", "main")).await;
            assert!(
                matches!(handled, Handled::Started(_)),
                "{case}: {handled:?}"
            );
            rebases_recorded(&f.app, 2).await;
        }
    }

    #[tokio::test]
    async fn one_push_rebases_no_more_than_twenty_pull_requests() {
        let behind: Vec<BehindPull> = (1..=25).map(|n| pull(n, 0x30)).collect();
        let f = fixture(CONFIG, behind, false);
        let handled = f.listener.handle(push("o/r", "main")).await;
        assert!(matches!(handled, Handled::Started(_)), "{handled:?}");
        rebases_recorded(&f.app, MOST_PER_PUSH).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(rebases(&f.app).await.len(), MOST_PER_PUSH);
    }
}
