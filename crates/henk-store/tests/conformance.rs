//! What every [`RunStore`] backend must do. Each scenario runs against
//! `SQLite` always and against `PostgreSQL` when `HENK_TEST_DATABASE_URL` is
//! set (`cargo test -p henk-store -- --ignored`).

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use henk_store::{
    EventFilter, FindingAction, InboundEvent, LaneStatus, MAX_PAYLOAD_BYTES, NewRun, OutcomeRecord,
    Page, PgStore, PruneCounts, RunFilter, RunRecord, RunStatus, RunStore, SqliteStore,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn new_run(id: &str) -> NewRun {
    NewRun {
        id: RunId::parse(id).unwrap(),
        kind: RunKind::Review,
        platform: Platform::GitHub,
        repo: "o/r".into(),
        target: 7,
        commit: Some("abc".into()),
        requester: None,
        trigger: "opened".into(),
        link: format!("https://henk.example/runs/{id}"),
    }
}

fn id(value: &str) -> RunId {
    RunId::parse(value).unwrap()
}

fn event_id(value: &str) -> EventId {
    EventId::parse(value).unwrap()
}

fn inbound(id: &str, received_at: &str) -> InboundEvent {
    InboundEvent {
        id: event_id(id),
        received_at: received_at.into(),
        source: "github_webhook".into(),
        kind: "pull_request".into(),
        repo: Some("o/r".into()),
        target: Some(7),
        payload: Some("{}".into()),
        requester: None,
    }
}

fn outcome(event: &str, listener: &str, run: Option<&str>) -> OutcomeRecord {
    OutcomeRecord {
        event_id: event_id(event),
        listener: listener.into(),
        outcome: if run.is_some() { "started" } else { "ignored" }.into(),
        detail: "detail".into(),
        run_id: run.map(Into::into),
        at: String::new(),
    }
}

/// Every scenario, by name. Each backend module turns them into tests.
macro_rules! for_each_scenario {
    ($tests:ident) => {
        $tests!(
            run_round_trips,
            a_heartbeat_moves_and_a_check_id_is_kept,
            only_running_runs_with_a_stale_heartbeat_are_orphaned,
            dropping_running_lanes_leaves_finished_ones_alone,
            lanes_findings_and_events_attach_to_a_run,
            events_and_outcomes_round_trip,
            old_events_and_their_outcomes_are_pruned,
            outcomes_and_linked_events_keep_their_order,
            a_duplicate_run_id_is_an_error,
            joining_is_accepted,
            runs_are_listed_newest_first_by_filter_and_page,
            inbound_events_are_listed_with_their_outcomes,
            a_number_beyond_i64_is_refused_not_stored_as_something_else,
        );
    };
}

async fn old_events_and_their_outcomes_are_pruned(store: &dyn RunStore) {
    store.create_run(&new_run("r-pruned")).await.unwrap();
    let now = OffsetDateTime::now_utc();
    let recent = now.format(&Rfc3339).unwrap();
    store
        .record_event(&inbound("e-old", "2026-01-01T00:00:00Z"))
        .await
        .unwrap();
    store
        .record_event(&inbound("e-new", &recent))
        .await
        .unwrap();
    for (event, listener) in [
        ("e-old", "review"),
        ("e-old", "mention"),
        ("e-new", "review"),
    ] {
        store
            .record_outcome(&outcome(event, listener, Some("r-pruned")))
            .await
            .unwrap();
    }

    let counts = store
        .prune_events(now - time::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(
        counts,
        PruneCounts {
            events: 1,
            outcomes: 2
        }
    );
    assert!(
        store
            .inbound_event(&event_id("e-old"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.outcomes(&event_id("e-old")).await.unwrap().is_empty());
    assert!(
        store
            .inbound_event(&event_id("e-new"))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(store.outcomes(&event_id("e-new")).await.unwrap().len(), 1);
    assert!(
        store.run(&id("r-pruned")).await.unwrap().is_some(),
        "runs are kept: their links are posted on the platforms"
    );

    let again = store
        .prune_events(now - time::Duration::days(1))
        .await
        .unwrap();
    assert_eq!(again, PruneCounts::default(), "nothing left to prune");
}

async fn run_round_trips(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    let run = store.run(&id("r-1")).await.unwrap().unwrap();
    assert_eq!(run.kind, RunKind::Review);
    assert_eq!(run.platform, Platform::GitHub);
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.repo, "o/r");
    assert_eq!(run.target, 7);
    assert_eq!(run.commit.as_deref(), Some("abc"));
    assert_eq!(run.link, "https://henk.example/runs/r-1");
    assert!(OffsetDateTime::parse(&run.started_at, &Rfc3339).is_ok());
    assert!(run.finished_at.is_none());

    store
        .finish_run(&run.id, RunStatus::Finished, Some("No issues found."), None)
        .await
        .unwrap();
    let run = store.run(&run.id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.summary.as_deref(), Some("No issues found."));
    assert!(run.finished_at.is_some());
    assert!(store.run(&id("nope")).await.unwrap().is_none());
}

async fn a_heartbeat_moves_and_a_check_id_is_kept(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    let first = store.run(&id("r-1")).await.unwrap().unwrap();
    assert!(first.heartbeat_at.is_some(), "a new run starts alive");
    assert_eq!(first.check_id, None);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store.heartbeat(&id("r-1")).await.unwrap();
    store.set_check(&id("r-1"), "42").await.unwrap();
    let later = store.run(&id("r-1")).await.unwrap().unwrap();
    // Compared as times: RFC 3339 text drops trailing zeros, so it does not sort.
    let at = |text: Option<String>| OffsetDateTime::parse(&text.unwrap(), &Rfc3339).unwrap();
    assert!(at(later.heartbeat_at.clone()) > at(first.heartbeat_at.clone()));
    assert_eq!(later.check_id.as_deref(), Some("42"));
}

async fn only_running_runs_with_a_stale_heartbeat_are_orphaned(store: &dyn RunStore) {
    for run in ["r-a", "r-b", "r-done"] {
        store.create_run(&new_run(run)).await.unwrap();
    }
    store
        .finish_run(&id("r-done"), RunStatus::Finished, None, None)
        .await
        .unwrap();
    let ids = |runs: Vec<RunRecord>| {
        runs.into_iter()
            .map(|r| r.id.to_string())
            .collect::<Vec<_>>()
    };

    let an_hour_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
    assert!(store.orphaned_runs(an_hour_ago).await.unwrap().is_empty());
    let in_an_hour = OffsetDateTime::now_utc() + time::Duration::hours(1);
    assert_eq!(
        ids(store.orphaned_runs(in_an_hour).await.unwrap()),
        ["r-a", "r-b"],
        "a finished run is never orphaned; oldest first"
    );
}

async fn dropping_running_lanes_leaves_finished_ones_alone(store: &dyn RunStore) {
    store.create_run(&new_run("r-1")).await.unwrap();
    store.start_lane(&id("r-1"), "a", "m").await.unwrap();
    store.start_lane(&id("r-1"), "b", "m").await.unwrap();
    store
        .finish_lane(&id("r-1"), "a", LaneStatus::Finished, 3, 1, 1, None)
        .await
        .unwrap();
    store
        .drop_running_lanes(&id("r-1"), "interrupted")
        .await
        .unwrap();
    let lanes = store.lanes(&id("r-1")).await.unwrap();
    let a = lanes.iter().find(|l| l.name == "a").unwrap();
    let b = lanes.iter().find(|l| l.name == "b").unwrap();
    assert_eq!(a.status, LaneStatus::Finished);
    assert_eq!(a.error, None);
    assert_eq!(b.status, LaneStatus::Dropped);
    assert_eq!(b.error.as_deref(), Some("interrupted"));
}

async fn lanes_findings_and_events_attach_to_a_run(store: &dyn RunStore) {
    let run = new_run("r-2");
    store.create_run(&run).await.unwrap();
    store.start_lane(&run.id, "b", "model-y").await.unwrap();
    store.start_lane(&run.id, "a", "model-x").await.unwrap();
    store
        .finish_lane(&run.id, "a", LaneStatus::Finished, 4, 1000, 200, None)
        .await
        .unwrap();
    store
        .finish_lane(&run.id, "b", LaneStatus::Dropped, 1, 10, 0, Some("timeout"))
        .await
        .unwrap();
    store
        .record_finding(&run.id, "a", "src/x.rs", 12, "c1", FindingAction::Posted)
        .await
        .unwrap();
    store
        .record_finding(&run.id, "b", "src/y.rs", 3, "c2", FindingAction::Withdrawn)
        .await
        .unwrap();
    store.event(&run.id, "info", "started").await.unwrap();
    store.event(&run.id, "warn", "slow").await.unwrap();

    let lanes = store.lanes(&run.id).await.unwrap();
    assert_eq!(lanes.len(), 2);
    assert_eq!(lanes[0].name, "a", "lanes come back by name");
    assert_eq!(lanes[0].model, "model-x");
    assert_eq!(lanes[0].status, LaneStatus::Finished);
    assert_eq!(
        (
            lanes[0].turns,
            lanes[0].input_tokens,
            lanes[0].output_tokens
        ),
        (4, 1000, 200)
    );
    assert_eq!(lanes[1].error.as_deref(), Some("timeout"));

    let events = store.events(&run.id).await.unwrap();
    let messages: Vec<_> = events.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(messages, ["started", "slow"], "timeline is oldest first");
    assert_eq!(events[1].level, "warn");

    let findings = store.findings(&run.id).await.unwrap();
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].lane, "a");
    assert_eq!(findings[0].path, "src/x.rs");
    assert_eq!(findings[0].line, 12);
    assert_eq!(findings[0].comment_id, "c1");
    assert_eq!(findings[0].action, "posted");
    assert_eq!(findings[1].action, "withdrawn");
    assert!(store.findings(&id("r-none")).await.unwrap().is_empty());
}

async fn events_and_outcomes_round_trip(store: &dyn RunStore) {
    store.create_run(&new_run("r-9")).await.unwrap();
    let event = inbound("e-1", "2026-10-03T00:00:00Z");
    store.record_event(&event).await.unwrap();
    store
        .record_outcome(&outcome("e-1", "review", Some("r-9")))
        .await
        .unwrap();

    let read = store
        .inbound_event(&event_id("e-1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.kind, "pull_request");
    assert_eq!(read.source, "github_webhook");
    assert_eq!(read.repo.as_deref(), Some("o/r"));
    assert_eq!(read.target, Some(7));
    assert_eq!(read.payload.as_deref(), Some("{}"));
    assert_eq!(read.requester, None);
    let asked = InboundEvent {
        source: "dashboard".into(),
        requester: Some("github:1234".into()),
        ..inbound("e-asked", "2026-10-03T00:00:01Z")
    };
    store.record_event(&asked).await.unwrap();
    assert_eq!(
        store
            .inbound_event(&event_id("e-asked"))
            .await
            .unwrap()
            .unwrap()
            .requester
            .as_deref(),
        Some("github:1234"),
        "who asked is kept"
    );
    let received = OffsetDateTime::parse(&read.received_at, &Rfc3339).unwrap();
    assert_eq!(
        received.unix_timestamp(),
        1_790_985_600,
        "the given time is kept"
    );

    let outcomes = store.outcomes(&event_id("e-1")).await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].run_id.as_deref(), Some("r-9"));
    assert!(!outcomes[0].at.is_empty(), "an empty time means now");
    assert_eq!(
        store
            .inbound_events_for_run(&id("r-9"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .inbound_event(&event_id("e-nope"))
            .await
            .unwrap()
            .is_none()
    );

    let big = "x".repeat(MAX_PAYLOAD_BYTES + 1);
    store
        .record_event(&InboundEvent {
            id: event_id("e-2"),
            payload: Some(big),
            ..event
        })
        .await
        .unwrap();
    assert!(
        store
            .inbound_event(&event_id("e-2"))
            .await
            .unwrap()
            .unwrap()
            .payload
            .is_none(),
        "oversized payload dropped"
    );
}

async fn outcomes_and_linked_events_keep_their_order(store: &dyn RunStore) {
    store.create_run(&new_run("r-5")).await.unwrap();
    store
        .record_event(&inbound("e-late", "2026-10-03T00:00:02Z"))
        .await
        .unwrap();
    store
        .record_event(&inbound("e-early", "2026-10-03T00:00:01Z"))
        .await
        .unwrap();
    for listener in ["review", "mention", "plan"] {
        let run = (listener == "review").then_some("r-5");
        store
            .record_outcome(&outcome("e-late", listener, run))
            .await
            .unwrap();
    }
    // Two outcomes of one event point at the run: the event is listed once.
    store
        .record_outcome(&outcome("e-early", "review", Some("r-5")))
        .await
        .unwrap();
    store
        .record_outcome(&outcome("e-early", "mention", Some("r-5")))
        .await
        .unwrap();

    let listeners: Vec<_> = store
        .outcomes(&event_id("e-late"))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.listener)
        .collect();
    assert_eq!(listeners, ["review", "mention", "plan"], "recording order");
    let linked: Vec<_> = store
        .inbound_events_for_run(&id("r-5"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.id.to_string())
        .collect();
    assert_eq!(linked, ["e-early", "e-late"], "oldest first, each once");
}

async fn a_duplicate_run_id_is_an_error(store: &dyn RunStore) {
    store.create_run(&new_run("r-dup")).await.unwrap();
    assert!(store.create_run(&new_run("r-dup")).await.is_err());
    store
        .record_event(&inbound("e-dup", "2026-10-03T00:00:00Z"))
        .await
        .unwrap();
    assert!(
        store
            .record_event(&inbound("e-dup", "2026-10-03T00:00:00Z"))
            .await
            .is_err()
    );
}

async fn joining_is_accepted(store: &dyn RunStore) {
    store.create_run(&new_run("r-3")).await.unwrap();
    store.joined(&id("r-3"), "comment").await.unwrap();
    store.joined(&id("r-3"), "webhook").await.unwrap();
}

async fn a_number_beyond_i64_is_refused_not_stored_as_something_else(store: &dyn RunStore) {
    let run = NewRun {
        target: u64::MAX,
        ..new_run("r-big")
    };
    assert!(matches!(
        store.create_run(&run).await,
        Err(henk_store::StoreError::Corrupt { .. })
    ));
    assert!(store.run(&id("r-big")).await.unwrap().is_none());

    let event = InboundEvent {
        target: Some(u64::MAX),
        ..inbound("e-big", "2026-10-03T00:00:00Z")
    };
    assert!(matches!(
        store.record_event(&event).await,
        Err(henk_store::StoreError::Corrupt { .. })
    ));
    assert!(
        store
            .inbound_event(&event_id("e-big"))
            .await
            .unwrap()
            .is_none()
    );
}

async fn runs_are_listed_newest_first_by_filter_and_page(store: &dyn RunStore) {
    let plan = |id: &str| NewRun {
        kind: RunKind::Plan,
        repo: "o/other".into(),
        ..new_run(id)
    };
    for run in [new_run("r-1"), plan("r-2"), new_run("r-3"), new_run("r-4")] {
        store.create_run(&run).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    store
        .finish_run(&id("r-3"), RunStatus::Failed, None, Some("boom"))
        .await
        .unwrap();
    let ids = |runs: Vec<RunRecord>| {
        runs.into_iter()
            .map(|r| r.id.to_string())
            .collect::<Vec<_>>()
    };
    let all = RunFilter::default();

    assert_eq!(
        ids(store.list_runs(&all, Page::new(50, 0)).await.unwrap()),
        ["r-4", "r-3", "r-2", "r-1"]
    );
    assert_eq!(
        ids(store.list_runs(&all, Page::new(2, 0)).await.unwrap()),
        ["r-4", "r-3"]
    );
    assert_eq!(
        store.count_runs(&all).await.unwrap(),
        4,
        "the count spans every page"
    );
    assert_eq!(
        ids(store.list_runs(&all, Page::new(2, 2)).await.unwrap()),
        ["r-2", "r-1"]
    );
    assert!(
        store
            .list_runs(&all, Page::new(2, 4))
            .await
            .unwrap()
            .is_empty()
    );
    let failed = RunFilter {
        status: Some(RunStatus::Failed),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(store.list_runs(&failed, Page::new(50, 0)).await.unwrap()),
        ["r-3"]
    );
    let plans = RunFilter {
        kind: Some(RunKind::Plan),
        ..RunFilter::default()
    };
    assert_eq!(
        ids(store.list_runs(&plans, Page::new(50, 0)).await.unwrap()),
        ["r-2"]
    );
    let running_reviews = RunFilter {
        kind: Some(RunKind::Review),
        status: Some(RunStatus::Running),
        platform: Some(Platform::GitHub),
        repo: Some("o/r".into()),
    };
    assert_eq!(
        ids(store
            .list_runs(&running_reviews, Page::new(50, 0))
            .await
            .unwrap()),
        ["r-4", "r-1"]
    );
    assert_eq!(store.count_runs(&running_reviews).await.unwrap(), 2);
    assert_eq!(store.count_runs(&failed).await.unwrap(), 1);
    assert_eq!(store.count_runs(&plans).await.unwrap(), 1);
    let gitlab = RunFilter {
        platform: Some(Platform::GitLab),
        ..RunFilter::default()
    };
    assert!(
        store
            .list_runs(&gitlab, Page::new(50, 0))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.count_runs(&gitlab).await.unwrap(), 0);
    assert_eq!(Page::new(1000, 0).limit(), Page::MAX, "a page is capped");
    assert_eq!(Page::new(0, 0).limit(), 1);
}

async fn inbound_events_are_listed_with_their_outcomes(store: &dyn RunStore) {
    store.create_run(&new_run("r-7")).await.unwrap();
    store
        .record_event(&inbound("e-old", "2026-10-03T00:00:01Z"))
        .await
        .unwrap();
    store
        .record_event(&inbound("e-new", "2026-10-03T00:00:02Z"))
        .await
        .unwrap();
    let api = InboundEvent {
        source: "api".into(),
        kind: "plan_requested".into(),
        ..inbound("e-api", "2026-10-03T00:00:03Z")
    };
    store.record_event(&api).await.unwrap();
    store
        .record_outcome(&outcome("e-new", "review", Some("r-7")))
        .await
        .unwrap();
    store
        .record_outcome(&outcome("e-new", "mention", None))
        .await
        .unwrap();
    store
        .record_outcome(&outcome("e-old", "review", None))
        .await
        .unwrap();

    let listed = store
        .list_inbound_events(&EventFilter::default(), Page::new(50, 0))
        .await
        .unwrap();
    let ids: Vec<_> = listed.iter().map(|e| e.event.id.to_string()).collect();
    assert_eq!(ids, ["e-api", "e-new", "e-old"]);
    assert!(listed[0].outcomes.is_empty());
    let listeners: Vec<_> = listed[1]
        .outcomes
        .iter()
        .map(|o| o.listener.as_str())
        .collect();
    assert_eq!(
        listeners,
        ["review", "mention"],
        "each event gets its own, in order"
    );
    assert_eq!(listed[1].outcomes[0].run_id.as_deref(), Some("r-7"));
    assert_eq!(listed[2].outcomes.len(), 1);

    let webhooks = EventFilter {
        source: Some("github_webhook".into()),
        ..EventFilter::default()
    };
    let listed = store
        .list_inbound_events(&webhooks, Page::new(1, 1))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].event.id.to_string(), "e-old");
    let plans = EventFilter {
        kind: Some("plan_requested".into()),
        repo: Some("o/r".into()),
        ..EventFilter::default()
    };
    assert_eq!(
        store
            .list_inbound_events(&plans, Page::new(50, 0))
            .await
            .unwrap()
            .len(),
        1
    );
}

mod sqlite {
    use super::*;

    fn open() -> Arc<dyn RunStore> {
        Arc::new(SqliteStore::in_memory().unwrap())
    }

    macro_rules! tests {
        ($($scenario:ident),* $(,)?) => {
            $(
                #[tokio::test]
                async fn $scenario() {
                    super::$scenario(open().as_ref()).await;
                }
            )*
        };
    }

    for_each_scenario!(tests);
}

/// `PostgreSQL`, when `HENK_TEST_DATABASE_URL` names a server. Every test gets
/// its own schema, so tests run in parallel and leave nothing behind.
mod postgres {
    use super::*;

    const URL: &str = "HENK_TEST_DATABASE_URL";

    /// A store in a fresh schema; the schema is dropped with the guard.
    pub(super) struct Schema {
        pub(super) store: Arc<dyn RunStore>,
        pub(super) url: String,
        name: String,
        base: String,
    }

    impl Drop for Schema {
        fn drop(&mut self) {
            let (base, name) = (self.base.clone(), self.name.clone());
            // Drop runs outside the test's runtime: clean up on a thread.
            let _ = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    if let Ok(client) = plain_client(&base).await {
                        let _ = client
                            .batch_execute(&format!("DROP SCHEMA {name} CASCADE"))
                            .await;
                    }
                });
            })
            .join();
        }
    }

    async fn plain_client(url: &str) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
        tokio::spawn(connection);
        Ok(client)
    }

    pub(super) async fn schema() -> Schema {
        let base = std::env::var(URL).unwrap_or_else(|_| panic!("{URL} is not set"));
        let name = format!(
            "henk_test_{}",
            OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .unsigned_abs()
                ^ u128::from(std::process::id())
        );
        plain_client(&base)
            .await
            .unwrap()
            .batch_execute(&format!("CREATE SCHEMA {name}"))
            .await
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{name}");
        let store = Arc::new(PgStore::connect(&url).await.unwrap());
        Schema {
            store,
            url,
            name,
            base,
        }
    }

    macro_rules! tests {
        ($($scenario:ident),* $(,)?) => {
            $(
                #[tokio::test]
                #[ignore = "needs HENK_TEST_DATABASE_URL"]
                async fn $scenario() {
                    let schema = schema().await;
                    super::$scenario(schema.store.as_ref()).await;
                }
            )*
        };
    }

    for_each_scenario!(tests);

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn migrating_again_changes_nothing_and_concurrent_starts_agree() {
        let schema = schema().await;
        schema.store.create_run(&new_run("r-kept")).await.unwrap();
        let (a, b) = tokio::join!(PgStore::connect(&schema.url), PgStore::connect(&schema.url));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert!(
            a.run(&id("r-kept")).await.unwrap().is_some(),
            "data survives"
        );
        assert!(b.run(&id("r-kept")).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn a_run_that_never_had_a_heartbeat_is_orphaned() {
        let schema = schema().await;
        schema.store.create_run(&new_run("r-silent")).await.unwrap();
        plain_client(&schema.url)
            .await
            .unwrap()
            .batch_execute("UPDATE runs SET heartbeat_at = NULL WHERE id = 'r-silent'")
            .await
            .unwrap();
        let an_hour_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let orphans = schema.store.orphaned_runs(an_hour_ago).await.unwrap();
        assert_eq!(orphans.len(), 1);
    }
}

#[tokio::test]
async fn a_bad_url_is_refused_without_quoting_it() {
    let error = PgStore::connect("postgres://henk:hunter2@[::1")
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("hunter2"), "{error}");
}

#[test]
fn a_url_is_described_without_its_credentials() {
    let place =
        henk_store::describe_url("postgres://henk:hunter2@db.internal:5433/henk?sslmode=require")
            .unwrap();
    assert_eq!(place, "db.internal:5433/henk");
}
