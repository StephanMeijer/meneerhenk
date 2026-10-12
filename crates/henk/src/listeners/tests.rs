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
use crate::push::PushedFor;
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
    own_pushes: crate::push::OwnPushes,
    coordinator: Arc<Coordinator>,
    next: std::sync::atomic::AtomicU32,
}

impl Harness {
    async fn new() -> Self {
        Self::with_writer(FakeWriter {
            head: SHA.to_owned(),
            ..FakeWriter::default()
        })
        .await
    }

    async fn with_writer(writer: FakeWriter) -> Self {
        let settings = Config::parse(CONFIG).unwrap().into_settings().unwrap();
        let app = Arc::new(
            App::build(settings, Some(Path::new(":memory:")))
                .await
                .unwrap(),
        );
        let writer = Arc::new(writer);
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
                Arc::new(AddressListener::new(Arc::clone(&coordinator))),
            ],
        );
        Self {
            bus,
            store: Arc::clone(&app.store),
            writer,
            own_pushes: app.own_pushes.clone(),
            coordinator,
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
async fn new_commits_henk_pushed_himself_start_no_review() {
    let h = Harness::new().await;
    h.own_pushes
        .record(&SHA.to_ascii_uppercase(), PushedFor::ReviewedByRun);
    // Sent by a person's account, so only the pushed commit tells (#284).
    let out = h
        .deliver(pull_request("docspec/app", person("alice"), false))
        .await;
    assert!(
        matches!(out.of("review"), Handled::Ignored(r) if r == "Henk's own push"),
        "{:?}",
        out.by_listener
    );
}

/// Henk's App pushed it, so the sender is a bot; the commit says it is an
/// address run's, which is reviewed as usual (§3.5, #298).
fn henks_push() -> Sender {
    Sender {
        login: "meneer-henk[bot]".into(),
        is_bot: true,
    }
}

#[tokio::test]
async fn henks_address_commit_is_reviewed_though_a_bot_pushed_it() {
    let h = Harness::new().await;
    let before = h
        .deliver(pull_request("docspec/app", henks_push(), false))
        .await;
    assert!(
        matches!(before.of("review"), Handled::Ignored(r) if r == "sent by a bot"),
        "a bot's commit Henk did not push stays filtered: {:?}",
        before.by_listener
    );

    h.own_pushes.record(SHA, PushedFor::Review);
    let out = h
        .deliver(pull_request("docspec/app", henks_push(), false))
        .await;
    assert!(
        matches!(out.of("review"), Handled::Started(_)),
        "{:?}",
        out.by_listener
    );
    let foreign = h
        .deliver(pull_request("evil/app", henks_push(), false))
        .await;
    assert!(
        matches!(foreign.of("review"), Handled::Ignored(r) if r.contains("allowlist")),
        "the allowlist still applies: {:?}",
        foreign.by_listener
    );
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

/// A review a restart interrupted, as the store keeps it: of an older
/// commit than the pull request's head now.
async fn interrupted(h: &Harness, run: &str, path: &str) -> henk_store::RunRecord {
    let id = RunId::parse(run).unwrap();
    h.store
        .create_run(&henk_store::NewRun {
            id: id.clone(),
            kind: henk_domain::run::RunKind::Review,
            platform: Platform::GitHub,
            repo: path.to_owned(),
            target: 7,
            commit: Some("fedcba9876543210fedcba9876543210fedcba98".to_owned()),
            requester: Some("alice".to_owned()),
            trigger: "new commits".to_owned(),
            link: format!("http://henk/runs/{run}"),
        })
        .await
        .unwrap();
    h.store
        .finish_run(
            &id,
            henk_store::RunStatus::Failed,
            None,
            Some("interrupted"),
        )
        .await
        .unwrap();
    h.store.run(&id).await.unwrap().unwrap()
}

/// What the review listener did with `event`.
async fn reviewed(h: &Harness, event: Event) -> Handled {
    h.bus
        .deliver(event)
        .await
        .into_iter()
        .find(|(name, _)| *name == "review")
        .unwrap()
        .1
}

/// #160: a resumed review goes through the review listener like any
/// request, reviews the pull request's current head, not the interrupted
/// commit, and says which run it resumes.
#[tokio::test]
async fn a_resumed_review_is_of_the_current_head_and_names_the_interrupted_run() {
    let h = Harness::new().await;
    // Every slot taken, so the resumed review waits where it can be seen.
    let mut held = Vec::new();
    while let Some(slot) = h.coordinator.take_a_slot() {
        held.push(slot);
    }
    let old = interrupted(&h, "r-old", "docspec/app").await;
    let event = crate::resume::resume_event(&old).unwrap();
    let id = event.id.clone();
    let Handled::Started(run) = reviewed(&h, event).await else {
        panic!("the resumed review did not start");
    };
    assert_ne!(
        run.as_str(),
        "r-old",
        "a resumed review is a run of its own"
    );
    let slots = h.coordinator.slots();
    assert_eq!(slots.waiting.len(), 1);
    let waiting = &slots.waiting[0];
    assert_eq!(waiting.run, run);
    assert_eq!(waiting.commit.as_str(), SHA, "the current head");
    assert_eq!(waiting.trigger, "resumed after interrupted run r-old");
    assert_eq!(waiting.requester.as_deref(), Some("alice"));
    let recorded = h.store.inbound_event(&id).await.unwrap().unwrap();
    assert_eq!(recorded.source, "resume");
    assert_eq!(recorded.repo.as_deref(), Some("docspec/app"));
}

/// A start's first resume pass, on a bus nobody listens to: it claims
/// the interrupted reviews and says how many.
async fn claim_on_start(app: &App) -> usize {
    let nobody = EventBus::new(Arc::new(henk_events::bus::NoRecorder), Vec::new());
    crate::resume::Resumer::new(Arc::new(nobody), std::time::Duration::from_hours(1))
        .after_pass(app, tokio::time::Instant::now())
        .await
}

/// #160: a resume whose pull request could not be read, as in an outage
/// right after a start, gives back its claim, so a later pass or start
/// resumes the review; it is not lost.
#[tokio::test]
async fn a_resume_that_could_not_read_the_pull_request_is_tried_again() {
    let h = Harness::with_writer(FakeWriter {
        head: SHA.to_owned(),
        fail_pull_request: Some("unreachable".to_owned()),
        ..FakeWriter::default()
    })
    .await;
    let app = h.coordinator.app();
    interrupted(&h, "r-old", "docspec/app").await;
    // The resumer claims it; its request is delivered below, by hand.
    assert_eq!(claim_on_start(app).await, 1);
    let since = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    assert!(
        crate::resume::interrupted_reviews(app, since)
            .await
            .is_empty(),
        "claimed"
    );

    let old = h
        .store
        .run(&RunId::parse("r-old").unwrap())
        .await
        .unwrap()
        .unwrap();
    let review = reviewed(&h, crate::resume::resume_event(&old).unwrap()).await;
    assert!(matches!(review, Handled::Failed(_)), "{review:?}");
    let again = crate::resume::interrupted_reviews(app, since).await;
    assert_eq!(
        again.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["r-old"],
        "resumable again"
    );
    assert_eq!(again[0].error.as_deref(), Some("interrupted"));
}

/// #160: a pull request that left the allowlist or was closed is not
/// resumed.
#[tokio::test]
async fn a_resumed_review_is_refused_off_the_allowlist_or_on_a_closed_pull_request() {
    let h = Harness::new().await;
    let outside = interrupted(&h, "r-outside", "evil/app").await;
    let review = reviewed(&h, crate::resume::resume_event(&outside).unwrap()).await;
    assert!(
        matches!(&review, Handled::Ignored(r) if r.contains("allowlist")),
        "{review:?}"
    );

    let closed = Harness::with_writer(FakeWriter {
        head: SHA.to_owned(),
        state: Some(henk_platform::PullRequestState::Closed),
        ..FakeWriter::default()
    })
    .await;
    let old = interrupted(&closed, "r-closed", "docspec/app").await;
    let app = closed.coordinator.app();
    assert_eq!(claim_on_start(app).await, 1);
    let review = reviewed(&closed, crate::resume::resume_event(&old).unwrap()).await;
    assert!(
        matches!(&review, Handled::Ignored(r) if r.contains("not open")),
        "{review:?}"
    );
    let since = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    assert!(
        crate::resume::interrupted_reviews(app, since)
            .await
            .is_empty(),
        "a refusal keeps the claim"
    );
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
        live_runs: crate::liveness::LiveRuns::default(),
        feed: crate::live::Feed::default(),
        cancels: crate::cancel::Cancels::default(),
        workspace_provider: std::sync::Arc::new(crate::workspace::host::HostProvider),
        own_pushes: crate::push::OwnPushes::default(),
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
            .submit_address(target.clone(), None, "a".into(), None)
            .is_ok()
    );
    let second = coordinator.submit_address(target.clone(), None, "b".into(), None);
    assert!(matches!(second, Err(r) if r.contains("already going")));
    let other = ReviewTarget {
        repo: repo("docspec/app"),
        number: 8,
    };
    assert!(
        coordinator
            .submit_address(other, None, "c".into(), None)
            .is_ok(),
        "another pull request may"
    );
}
