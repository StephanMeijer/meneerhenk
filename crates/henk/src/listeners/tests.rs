//! In-process listener tests: a synthetic event goes through the bus and the
//! outcomes, the store and the fake writer show what happened. No HTTP.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::sync::Arc;

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::plan::IssueTarget;
use henk_domain::review::{CommitSha, ReviewTarget};
use henk_domain::run::{EventId, RunId};
use henk_events::{
    CommentKind, Event, EventBus, EventKind, EventSource, Handled, PullRequestAction, Sender,
};
use henk_store::RunStore;

use super::testing::{FakeWriter, FakeWriters};
use super::{AddressListener, MentionListener, PlanListener, ReviewListener, Writers};
use crate::app::App;
use crate::config::Config;
use crate::coordinator::Coordinator;
use crate::recorder::StoreRecorder;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const CONFIG: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
"#;

struct Harness {
    bus: EventBus,
    store: Arc<dyn RunStore>,
    writer: Arc<FakeWriter>,
    next: std::sync::atomic::AtomicU32,
}

impl Harness {
    async fn new() -> Self {
        let settings = Config::parse(CONFIG).unwrap().into_settings().unwrap();
        let app = Arc::new(
            App::build(settings, Some(Path::new(":memory:")))
                .await
                .unwrap(),
        );
        let writer = Arc::new(FakeWriter {
            head: SHA.to_owned(),
            ..FakeWriter::default()
        });
        let writers: Arc<dyn Writers> = Arc::new(FakeWriters(Arc::clone(&writer)));
        let coordinator = Arc::new(Coordinator::new(Arc::clone(&app)));
        let bus = EventBus::new(
            Arc::new(StoreRecorder(Arc::clone(&app.store))),
            vec![
                Arc::new(ReviewListener::new(
                    Arc::clone(&coordinator),
                    Arc::clone(&writers),
                )),
                Arc::new(MentionListener::new(
                    Arc::new(app.settings.clone()),
                    writers,
                )),
                Arc::new(PlanListener::new(Arc::clone(&coordinator))),
                Arc::new(AddressListener::new(coordinator)),
            ],
        );
        Self {
            bus,
            store: Arc::clone(&app.store),
            writer,
            next: std::sync::atomic::AtomicU32::new(1),
        }
    }

    fn event(&self, kind: EventKind) -> Event {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Event {
            id: EventId::parse(format!("e-{n}")).unwrap(),
            received_at: "2026-10-03T00:00:00Z".into(),
            source: EventSource::GitHubWebhook {
                delivery: format!("d{n}"),
            },
            kind,
            payload: None,
        }
    }

    async fn deliver(&self, kind: EventKind) -> Outcomes {
        let event = self.event(kind);
        let id = event.id.clone();
        let outcomes = self.bus.deliver(event).await;
        Outcomes {
            id,
            by_listener: outcomes,
        }
    }
}

struct Outcomes {
    id: EventId,
    by_listener: Vec<(&'static str, Handled)>,
}

impl Outcomes {
    fn of(&self, listener: &str) -> &Handled {
        &self
            .by_listener
            .iter()
            .find(|(n, _)| *n == listener)
            .unwrap()
            .1
    }
}

fn repo(path: &str) -> RepoRef {
    RepoRef::parse(Platform::GitHub, path).unwrap()
}

fn person(login: &str) -> Sender {
    Sender {
        login: login.into(),
        is_bot: false,
    }
}

fn pull_request(path: &str, sender: Sender, draft: bool) -> EventKind {
    EventKind::PullRequest {
        repo: repo(path),
        number: 7,
        action: PullRequestAction::Synchronized,
        head: CommitSha::parse(SHA).unwrap(),
        draft,
        sender,
    }
}

fn comment(body: &str, sender: Sender) -> EventKind {
    EventKind::Comment {
        repo: repo("docspec/app"),
        number: 7,
        body: body.into(),
        comment_id: "99".into(),
        kind: CommentKind::Conversation,
        sender,
    }
}

#[tokio::test]
async fn bots_markers_and_foreign_repositories_are_ignored_by_everyone() {
    let h = Harness::new().await;
    let bot = h
        .deliver(pull_request(
            "docspec/app",
            Sender {
                login: "x[bot]".into(),
                is_bot: true,
            },
            false,
        ))
        .await;
    assert!(matches!(bot.of("review"), Handled::Ignored(r) if r == "sent by a bot"));

    let marker = Marker {
        run: RunId::parse("r-0").unwrap(),
        model: ModelId::parse("m").unwrap(),
        requested_by: None,
        kind: Some(MarkerKind::Summary),
        checked_by: None,
        withdrawn: None,
    }
    .attach("@meneer-henk review");
    let own = h.deliver(comment(&marker, person("alice"))).await;
    assert!(matches!(own.of("review"), Handled::Ignored(r) if r.contains("markers")));
    assert!(matches!(own.of("mention"), Handled::Ignored(r) if r.contains("markers")));

    let foreign = h
        .deliver(pull_request("evil/app", person("alice"), false))
        .await;
    assert!(matches!(foreign.of("review"), Handled::Ignored(r) if r.contains("allowlist")));
    assert!(h.writer.replies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn drafts_wait_until_ready() {
    let h = Harness::new().await;
    let draft = h
        .deliver(pull_request("docspec/app", person("alice"), true))
        .await;
    assert!(matches!(draft.of("review"), Handled::Ignored(r) if r.starts_with("draft")));
}

#[tokio::test]
async fn a_pull_request_change_starts_a_review_and_is_recorded() {
    let h = Harness::new().await;
    let out = h
        .deliver(pull_request("docspec/app", person("alice"), false))
        .await;
    let Handled::Started(run) = out.of("review") else {
        panic!("{:?}", out.by_listener)
    };
    assert!(matches!(out.of("mention"), Handled::Ignored(_)));
    assert!(matches!(out.of("plan"), Handled::Ignored(_)));

    let recorded = h.store.inbound_event(&out.id).await.unwrap().unwrap();
    assert_eq!(recorded.kind, "pull_request");
    assert_eq!(recorded.repo.as_deref(), Some("docspec/app"));
    assert_eq!(recorded.target, Some(7));
    let outcomes = h.store.outcomes(&out.id).await.unwrap();
    assert_eq!(outcomes.len(), 4);
    let started = outcomes.iter().find(|o| o.listener == "review").unwrap();
    assert_eq!(started.outcome, "started");
    assert_eq!(started.run_id.as_deref(), Some(run.as_str()));
}

#[tokio::test]
async fn a_review_command_starts_exactly_one_review_and_no_greeting() {
    let h = Harness::new().await;
    let out = h
        .deliver(comment("@meneer-henk review", person("alice")))
        .await;
    assert!(
        matches!(out.of("review"), Handled::Started(_)),
        "{:?}",
        out.by_listener
    );
    assert!(matches!(out.of("mention"), Handled::Ignored(r) if r.contains("command")));
    assert_eq!(
        *h.writer.pull_request_calls.lock().unwrap(),
        1,
        "head resolved once"
    );
    assert!(
        h.writer.replies.lock().unwrap().is_empty(),
        "no greeting for a command"
    );
}

#[tokio::test]
async fn a_plain_mention_gets_one_greeting_with_a_marker() {
    let h = Harness::new().await;
    let out = h
        .deliver(comment("Thanks @meneer-henk, good catch.", person("alice")))
        .await;
    assert!(
        matches!(out.of("mention"), Handled::Greeted),
        "{:?}",
        out.by_listener
    );
    assert!(matches!(out.of("review"), Handled::Ignored(r) if r.contains("not a review command")));
    let replies = h.writer.replies.lock().unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].comment_id, "99");
    assert!(!replies[0].is_review_comment);
    let marker = Marker::parse(&replies[0].body).unwrap();
    assert_eq!(marker.kind, Some(MarkerKind::Reply));
    assert!(henk_domain::text::is_in_style(&replies[0].body));
}

#[tokio::test]
async fn a_direct_request_without_a_commit_resolves_the_head() {
    let h = Harness::new().await;
    let out = h
        .deliver(EventKind::ReviewRequested {
            target: ReviewTarget {
                repo: repo("docspec/app"),
                number: 7,
            },
            commit: None,
            requester: Some("523".into()),
        })
        .await;
    assert!(
        matches!(out.of("review"), Handled::Started(_)),
        "{:?}",
        out.by_listener
    );
    assert_eq!(*h.writer.pull_request_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_plan_request_starts_a_plan() {
    let h = Harness::new().await;
    let out = h
        .deliver(EventKind::PlanRequested {
            target: IssueTarget {
                repo: repo("docspec/app"),
                number: 9,
            },
            note: None,
            requester: Some("523".into()),
        })
        .await;
    assert!(
        matches!(out.of("plan"), Handled::Started(_)),
        "{:?}",
        out.by_listener
    );
    assert!(matches!(out.of("review"), Handled::Ignored(_)));
    let outside = h
        .deliver(EventKind::PlanRequested {
            target: IssueTarget {
                repo: repo("evil/app"),
                number: 9,
            },
            note: None,
            requester: None,
        })
        .await;
    assert!(matches!(outside.of("plan"), Handled::Ignored(r) if r.contains("allowlist")));
}

#[tokio::test]
async fn an_address_request_needs_address_runs_configured() {
    let h = Harness::new().await;
    let out = h
        .deliver(EventKind::AddressRequested {
            target: ReviewTarget {
                repo: repo("docspec/app"),
                number: 7,
            },
            note: None,
            requester: Some("523".into()),
        })
        .await;
    assert!(
        matches!(out.of("address"), Handled::Ignored(r) if r.contains("not configured")),
        "{:?}",
        out.by_listener
    );
    assert!(matches!(out.of("review"), Handled::Ignored(_)));
    assert!(matches!(out.of("plan"), Handled::Ignored(_)));
}

#[tokio::test]
async fn one_address_run_per_pull_request_at_a_time() {
    let text = format!(
        "{CONFIG}[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"UNUSED\"\nmodel = \"x\"\n[address]\nmodel = \"m\"\nrequester_id = 3\n"
    );
    let settings = Config::parse(&text).unwrap().into_settings().unwrap();
    let app = Arc::new(App {
        settings,
        store: Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
        models: std::collections::BTreeMap::new(),
        github: None,
        gitlab: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        test_writer: None,
        test_session: None,
        test_address_writer: None,
        test_issue_writer: None,
    });
    let coordinator = Coordinator::new(app);
    let target = ReviewTarget {
        repo: repo("docspec/app"),
        number: 7,
    };
    // Both before the first run gets to start: the second is refused.
    assert!(
        coordinator
            .submit_address(target.clone(), None, "a".into())
            .is_ok()
    );
    let second = coordinator.submit_address(target.clone(), None, "b".into());
    assert!(matches!(second, Err(r) if r.contains("already going")));
    let other = ReviewTarget {
        repo: repo("docspec/app"),
        number: 8,
    };
    assert!(
        coordinator.submit_address(other, None, "c".into()).is_ok(),
        "another pull request may"
    );
}
