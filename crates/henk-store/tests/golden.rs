//! Golden fixtures of what the run store returns (#601): one dataset of
//! rows with fixed times, read back through the `RunStore` trait, rendered
//! as plain text and compared with files kept next to the tests.
//!
//! They are the reference for "the same results as before" when the store
//! moves onto another database layer (#598): the new code has to render
//! exactly what the old code rendered here, on SQLite in the plain test
//! run and on `PostgreSQL` when `HENK_TEST_DATABASE_URL` names a server
//! (`cargo test -p henk-store --test golden -- --ignored`). CI has no
//! `PostgreSQL`; its goldens are recorded and checked by hand.
//!
//! `HENK_BLESS=1` writes the files instead of comparing with them. Each
//! database has its own directory, because the two do not return alike
//! today. The goldens record what the stores do, including where they
//! differ:
//!
//! - A time. `PostgreSQL` keeps microseconds (`...00.123457Z`); SQLite keeps
//!   the text it was given (`...00.123456789Z`).
//! - The order of inbound events. SQLite orders `received_at` as text, so
//!   `...:00.4Z`, a later instant, sorts before `...:00Z` and is listed
//!   after it (the run listing was fixed for this in #216; the inbound events
//!   were not). `PostgreSQL` orders by time. `inbound_events.txt` shows it.
//!
//! A store that returns the same on both is what #598 is after; when the
//! goldens of the two databases become one, these are the lines that move.
//!
//! The rows are `golden/seed.sql`, plain INSERTs with fixed times and no ids
//! (`PostgreSQL`'s identity columns refuse one), so the dataset does not
//! depend on the write path, which `conformance.rs` covers.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::fmt::{Debug, Write as _};
use std::path::{Path, PathBuf};

use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use henk_store::{
    Candidate, EventFilter, EventKey, OutcomeFilter, Page, PgStore, RunFilter, RunKey, RunRecord,
    RunStatus, RunStore, SessionRole, ToolCallFilter, ToolCallKey,
};
use sea_orm::{ConnectionTrait, Database, Statement};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The rows every golden is read from.
const SEED: &str = include_str!("golden/seed.sql");

/// How a test reaches a database behind the store's back, to put the rows
/// in. The store under test is not involved, so a store built on another
/// layer is seeded the same way, with an executor of its own.
trait Seeder {
    async fn execute(&self, sql: &str);
}

/// A time, as the trait takes it.
fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

fn run_id(text: &str) -> RunId {
    RunId::parse(text).unwrap()
}

/// A page that holds the whole dataset.
fn all() -> Page {
    Page::new(100, 0)
}

/// The runs of the seed, oldest first, for the reads that go run by run.
const RUNS: [&str; 15] = [
    "r-001", "r-002", "r-003", "r-004", "r-005", "r-006", "r-007", "r-008", "r-009", "r-010",
    "r-011", "r-012", "r-013", "r-014", "r-015",
];

/// A block of the report: a title, then what the store returned, one line
/// per record, as Rust prints it (`{:?}`), so a field a record gains shows
/// up as a change here.
#[derive(Default)]
struct Block(String);

impl Block {
    fn title(&mut self, title: impl std::fmt::Display) -> &mut Self {
        writeln!(self.0, "== {title}").unwrap();
        self
    }

    fn rows<T: Debug>(&mut self, rows: &[T]) -> &mut Self {
        writeln!(self.0, "({} rows)", rows.len()).unwrap();
        for row in rows {
            writeln!(self.0, "{row:?}").unwrap();
        }
        self
    }

    /// Runs as `id started_at status`, the part a filter changes.
    fn runs(&mut self, runs: &[RunRecord]) -> &mut Self {
        writeln!(self.0, "({} runs)", runs.len()).unwrap();
        for run in runs {
            writeln!(self.0, "{} {} {:?}", run.id, run.started_at, run.status).unwrap();
        }
        self
    }

    fn line(&mut self, text: impl std::fmt::Display) -> &mut Self {
        writeln!(self.0, "{text}").unwrap();
        self
    }
}

async fn list_runs_pages(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    let filter = RunFilter::default();
    out.title("every run, newest first")
        .rows(&store.list_runs(&filter, all()).await.unwrap());
    for offset in [0, 4, 8, 12, 16] {
        out.title(format!("limit 4, offset {offset}")).runs(
            &store
                .list_runs(&filter, Page::new(4, offset))
                .await
                .unwrap(),
        );
    }
    out.0
}

async fn list_runs_filters(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    let by = |f: &dyn Fn(&mut RunFilter)| {
        let mut filter = RunFilter::default();
        f(&mut filter);
        filter
    };
    let filters: Vec<(&str, RunFilter)> = vec![
        ("kind review", by(&|f| f.kind = Some(RunKind::Review))),
        ("kind plan", by(&|f| f.kind = Some(RunKind::Plan))),
        ("kind ticket", by(&|f| f.kind = Some(RunKind::Ticket))),
        (
            "status running",
            by(&|f| f.status = Some(RunStatus::Running)),
        ),
        (
            "status finished, kind review",
            by(&|f| {
                f.status = Some(RunStatus::Finished);
                f.kind = Some(RunKind::Review);
            }),
        ),
        (
            "platform gitlab",
            by(&|f| f.platform = Some(Platform::GitLab)),
        ),
        ("repo o/r", by(&|f| f.repo = Some("o/r".into()))),
        ("target 7", by(&|f| f.target = Some(7))),
        (
            "target 8, kind review",
            by(&|f| {
                f.target = Some(8);
                f.kind = Some(RunKind::Review);
            }),
        ),
        (
            "since the second after the tie",
            by(&|f| f.since = Some("2026-10-01T10:00:01Z".into())),
        ),
        (
            "until the second after the tie",
            by(&|f| f.until = Some("2026-10-01T10:00:01Z".into())),
        ),
        (
            "the second of the tie",
            by(&|f| {
                f.since = Some("2026-10-01T10:00:00Z".into());
                f.until = Some("2026-10-01T10:00:01Z".into());
            }),
        ),
        (
            "since midnight",
            by(&|f| f.since = Some("2026-10-04T00:00:00Z".into())),
        ),
        ("nothing matches", by(&|f| f.target = Some(999))),
    ];
    for (name, filter) in &filters {
        out.title(name)
            .line(format!("count {}", store.count_runs(filter).await.unwrap()))
            .runs(&store.list_runs(filter, all()).await.unwrap());
    }
    out.title("count with no filter")
        .line(store.count_runs(&RunFilter::default()).await.unwrap());
    out.0
}

async fn list_runs_keyset(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    for (name, base) in [
        ("every run", RunFilter::default()),
        (
            "reviews",
            RunFilter {
                kind: Some(RunKind::Review),
                ..RunFilter::default()
            },
        ),
        (
            "o/r#7",
            RunFilter {
                repo: Some("o/r".into()),
                target: Some(7),
                ..RunFilter::default()
            },
        ),
    ] {
        let mut before: Option<RunKey> = None;
        for page in 1..=10 {
            let filter = RunFilter {
                before: before.clone(),
                ..base.clone()
            };
            let runs = store.list_runs(&filter, Page::new(3, 0)).await.unwrap();
            out.title(format!("{name}, page {page} of 3 after {before:?}"))
                .runs(&runs);
            let Some(last) = runs.last() else { break };
            before = Some(RunKey {
                started_at: last.started_at.clone(),
                id: last.id.as_str().to_owned(),
            });
        }
    }
    out.0
}

async fn liveness_and_reviews(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    for since in [
        "2026-10-01T00:00:00Z",
        "2026-10-01T10:00:01Z",
        "2026-10-03T00:00:00Z",
        "2026-10-05T00:00:00Z",
    ] {
        out.title(format!(
            "the newest review of each pull request since {since}"
        ))
        .rows(&store.latest_reviews(at(since)).await.unwrap());
    }
    for cutoff in [
        "2026-10-01T10:00:10Z",
        "2026-10-02T11:00:00Z",
        "2026-10-02T11:00:10Z",
    ] {
        out.title(format!("running runs with no heartbeat since {cutoff}"))
            .runs(&store.orphaned_runs(at(cutoff)).await.unwrap());
    }
    out.0
}

async fn days(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    for since in [
        "2026-10-01T00:00:00Z",
        "2026-10-03T00:00:00Z",
        "2026-10-04T00:00:00Z",
    ] {
        out.title(format!("days since {since}"))
            .rows(&store.daily_stats(at(since)).await.unwrap());
    }
    out.0
}

async fn tools(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    let by = |f: &dyn Fn(&mut ToolCallFilter)| {
        let mut filter = ToolCallFilter::default();
        f(&mut filter);
        filter
    };
    let filters: Vec<(&str, ToolCallFilter)> = vec![
        ("every call", ToolCallFilter::default()),
        (
            "outcome ok",
            by(&|f| f.outcome = Some(OutcomeFilter::Is("ok".into()))),
        ),
        (
            "any outcome but ok",
            by(&|f| f.outcome = Some(OutcomeFilter::Problems)),
        ),
        ("tool read_file", by(&|f| f.tool = Some("read_file".into()))),
        ("model gpt-m", by(&|f| f.model = Some("gpt-m".into()))),
        (
            "role executor",
            by(&|f| f.role = Some(SessionRole::Executor)),
        ),
        ("role lane", by(&|f| f.role = Some(SessionRole::Lane))),
        (
            "since the 2nd of October",
            by(&|f| f.since = Some("2026-10-02T00:00:00Z".into())),
        ),
        (
            "until the 2nd of October",
            by(&|f| f.until = Some("2026-10-02T00:00:00Z".into())),
        ),
    ];
    for (name, filter) in &filters {
        out.title(format!("usage, {name}"))
            .rows(&store.tool_usage(filter).await.unwrap());
        out.title(format!("calls, {name}"))
            .rows(&store.list_tool_calls(filter, all()).await.unwrap());
    }
    out.title("usage since the 2nd of October").rows(
        &store
            .tool_usage_since(at("2026-10-02T00:00:00Z"))
            .await
            .unwrap(),
    );
    // Paged by keyset, three at a time.
    let mut before: Option<ToolCallKey> = None;
    for page in 1..=8 {
        let filter = ToolCallFilter {
            before: before.clone(),
            ..ToolCallFilter::default()
        };
        let calls = store
            .list_tool_calls(&filter, Page::new(3, 0))
            .await
            .unwrap();
        out.title(format!("calls, page {page} of 3 after {before:?}"))
            .rows(&calls);
        let Some(last) = calls.last() else { break };
        before = Some(ToolCallKey {
            at: last.call.at.clone(),
            id: last.id,
        });
    }
    out.0
}

async fn endings(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    for (since, reviews) in [
        (None, 10),
        (Some("2026-10-02T00:00:00Z"), 10),
        (Some("2026-10-01T10:00:01Z"), 10),
        (None, 3),
        (None, 0),
    ] {
        out.title(format!(
            "session endings since {since:?}, {reviews} reviews"
        ))
        .rows(&store.session_endings(since.map(at), reviews).await.unwrap());
    }
    out.0
}

async fn reads_by_run(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    for run in RUNS {
        let id = run_id(run);
        out.title(run)
            .rows(
                &store
                    .run(&id)
                    .await
                    .unwrap()
                    .into_iter()
                    .collect::<Vec<_>>(),
            )
            .line("-- sessions")
            .rows(&store.sessions(&id).await.unwrap())
            .line("-- stages")
            .rows(&store.stages(&id).await.unwrap())
            .line("-- rounds")
            .rows(&store.rounds(&id).await.unwrap())
            .line("-- findings")
            .rows(&store.findings(&id).await.unwrap())
            .line("-- drafts")
            .rows(&store.drafts(&id).await.unwrap())
            .line("-- events")
            .rows(&store.events(&id).await.unwrap())
            .line("-- tool calls")
            .rows(&store.tool_calls(&id).await.unwrap())
            .line("-- transcripts")
            .rows(&store.transcripts(&id).await.unwrap())
            .line("-- children")
            .rows(&store.children(&id).await.unwrap())
            .line("-- inbound events")
            .rows(&store.inbound_events_for_run(&id).await.unwrap());
    }
    out.title("the newest transcript of r-003's reviewer").rows(
        &store
            .transcript(&run_id("r-003"), "reviewer")
            .await
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
    );
    out.title("no transcript").rows(
        &store
            .transcript(&run_id("r-002"), "nobody")
            .await
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
    );
    let all: Vec<RunId> = RUNS.iter().map(|run| run_id(run)).collect();
    out.title("the sessions of every run in one read")
        .rows(&store.sessions_of(&all).await.unwrap());
    out.title("the stages of every run in one read")
        .rows(&store.stages_of(&all).await.unwrap());
    out.title("the stages of no run")
        .rows(&store.stages_of(&[]).await.unwrap());
    out.0
}

async fn inbound(store: &dyn RunStore) -> String {
    let mut out = Block::default();
    let by = |f: &dyn Fn(&mut EventFilter)| {
        let mut filter = EventFilter::default();
        f(&mut filter);
        filter
    };
    let filters: Vec<(&str, EventFilter)> = vec![
        ("every event", EventFilter::default()),
        ("source api", by(&|f| f.source = Some("api".into()))),
        ("kind comment", by(&|f| f.kind = Some("comment".into()))),
        ("repo o/r", by(&|f| f.repo = Some("o/r".into()))),
        (
            "source github_webhook, kind pull_request",
            by(&|f| {
                f.source = Some("github_webhook".into());
                f.kind = Some("pull_request".into());
            }),
        ),
    ];
    for (name, filter) in &filters {
        out.title(name)
            .rows(&store.list_inbound_events(filter, all()).await.unwrap());
    }
    let mut before: Option<EventKey> = None;
    for page in 1..=6 {
        let filter = EventFilter {
            before: before.clone(),
            ..EventFilter::default()
        };
        let events = store
            .list_inbound_events(&filter, Page::new(3, 0))
            .await
            .unwrap();
        out.title(format!("events, page {page} of 3 after {before:?}"))
            .rows(&events);
        let Some(last) = events.last() else { break };
        before = Some(EventKey {
            received_at: last.event.received_at.clone(),
            id: last.event.id.as_str().to_owned(),
        });
    }
    out.title("the sources and kinds seen")
        .line(format!("{:?}", store.event_facets().await.unwrap()));
    let e001 = EventId::parse("e-001").unwrap();
    out.title("event e-001")
        .rows(
            &store
                .inbound_event(&e001)
                .await
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
        )
        .line("-- its outcomes")
        .rows(&store.outcomes(&e001).await.unwrap());
    out.title("no such event").rows(
        &store
            .inbound_event(&EventId::parse("e-999").unwrap())
            .await
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
    );
    out.0
}

/// Every block, by the name of its file.
async fn report(store: &dyn RunStore) -> Vec<(&'static str, String)> {
    vec![
        ("list_runs_pages", list_runs_pages(store).await),
        ("list_runs_filters", list_runs_filters(store).await),
        ("list_runs_keyset", list_runs_keyset(store).await),
        ("liveness_and_reviews", liveness_and_reviews(store).await),
        ("days", days(store).await),
        ("tools", tools(store).await),
        ("session_endings", endings(store).await),
        ("reads_by_run", reads_by_run(store).await),
        ("inbound_events", inbound(store).await),
    ]
}

/// Compares `report` with the files of `database`, or writes them when
/// `HENK_BLESS` is set.
fn check(database: &str, report: &[(&str, String)]) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(database);
    let bless = std::env::var_os("HENK_BLESS").is_some();
    if bless {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let mut stale = Vec::new();
    for (name, text) in report {
        let path = dir.join(format!("{name}.txt"));
        if bless {
            std::fs::write(&path, text).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}; record it with HENK_BLESS=1", path.display()));
        if &want != text {
            let (line, got, expected) = want
                .lines()
                .zip(text.lines())
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .map_or_else(
                    || {
                        (
                            want.lines().count().min(text.lines().count()) + 1,
                            "(end)",
                            "(end)",
                        )
                    },
                    |(n, (a, b))| (n + 1, b, a),
                );
            stale.push(format!(
                "{}: line {line} differs\n  golden: {expected}\n  store:  {got}",
                path.display()
            ));
        }
    }
    assert!(
        stale.is_empty(),
        "the store no longer returns what the goldens say (HENK_BLESS=1 records them again):\n{}",
        stale.join("\n")
    );
}

/// The SQLite file, through a second connection of its own (sea-orm's, not
/// the store's: rusqlite cannot share a build with this layer).
struct SqliteSeeder(PathBuf);

impl Seeder for SqliteSeeder {
    async fn execute(&self, sql: &str) {
        let db = Database::connect(format!("sqlite://{}?mode=rwc", self.0.display()))
            .await
            .unwrap();
        db.execute_unprepared(sql).await.unwrap();
        db.close().await.unwrap();
    }
}

/// One number out of a query, through a connection of its own.
async fn scalar(url: &str, sql: &str) -> i64 {
    let db = Database::connect(url).await.unwrap();
    let row = db
        .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .unwrap();
    let value: i64 = row.try_get_by_index(0).unwrap();
    db.close().await.unwrap();
    value
}

/// A database file under the temp directory, removed with the guard.
struct File(PathBuf);

impl File {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("henk-golden-{}-{n}.db", std::process::id())))
    }
}

impl Drop for File {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tokio::test]
async fn sqlite_returns_what_the_goldens_say() {
    let file = File::new();
    drop(Candidate::open(&file.0).await.unwrap());
    SqliteSeeder(file.0.clone()).execute(SEED).await;
    let store = Candidate::open(&file.0).await.unwrap();
    check("sqlite", &report(&store).await);
}

/// The fixture made by the old SQLite store at schema 020 and filled with
/// the seed (`PRAGMA user_version = 20`, no `seaql_migrations`).
const SQLITE_020: &str = "/tmp/claude-1000/-home-steve-Projects-github-com-StephanMeijer-meneerhenk/64411151-75e1-4b24-a999-21ad66170586/scratchpad/fixtures/sqlite-020-seeded.db";

#[tokio::test]
async fn sqlite_020_is_adopted_and_returns_what_the_goldens_say() {
    let file = File::new();
    std::fs::copy(SQLITE_020, &file.0).unwrap();
    let url = format!("sqlite://{}?mode=rwc", file.0.display());
    assert_eq!(scalar(&url, "PRAGMA user_version").await, 20);
    let store = Candidate::open(&file.0).await.unwrap();
    check("sqlite", &report(&store).await);
    // Adopted: the first three steps were recorded without running, the
    // fourth (new) one ran, and the old marker is as the old store left it.
    assert_eq!(
        scalar(&url, "SELECT count(*) FROM seaql_migrations").await,
        4
    );
    assert_eq!(
        scalar(
            &url,
            "SELECT count(*) FROM sqlite_master WHERE name IN ('sessions_run', 'findings_run', 'events_run')"
        )
        .await,
        3
    );
    assert_eq!(scalar(&url, "PRAGMA user_version").await, 20);
    // Opening it again changes nothing.
    drop(store);
    let store = Candidate::open(&file.0).await.unwrap();
    check("sqlite", &report(&store).await);
    assert_eq!(
        scalar(&url, "SELECT count(*) FROM seaql_migrations").await,
        4
    );
}

#[tokio::test]
async fn a_sqlite_file_between_001_and_019_is_refused_with_a_reason() {
    let file = File::new();
    let url = format!("sqlite://{}?mode=rwc", file.0.display());
    let db = Database::connect(&url).await.unwrap();
    db.execute_unprepared("CREATE TABLE runs (id TEXT); PRAGMA user_version = 13")
        .await
        .unwrap();
    db.close().await.unwrap();
    let error = Candidate::open(&file.0).await.unwrap_err();
    assert!(error.to_string().contains("schema version 13"), "{error}");
}

#[tokio::test]
async fn the_seed_is_the_same_rows_whichever_way_it_is_read() {
    // The goldens are only as good as their seed: every table it fills has
    // rows, and a second read of the same file gives the same report.
    let file = File::new();
    drop(Candidate::open(&file.0).await.unwrap());
    SqliteSeeder(file.0.clone()).execute(SEED).await;
    let first = report(&Candidate::open(&file.0).await.unwrap()).await;
    let second = report(&Candidate::open(&file.0).await.unwrap()).await;
    assert_eq!(first, second);
    let url = format!("sqlite://{}?mode=rwc", file.0.display());
    for table in [
        "runs",
        "sessions",
        "stages",
        "session_rounds",
        "findings",
        "drafts",
        "events",
        "tool_calls",
        "transcripts",
        "inbound_events",
        "event_outcomes",
        "run_children",
    ] {
        let rows = scalar(&url, &format!("SELECT count(*) FROM {table}")).await;
        assert!(rows > 0, "the seed puts nothing in {table}");
    }
}

/// Eight tasks, fifty writes each, through the layer's pool onto one SQLite
/// file: none may fail with "database is locked".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_writers_do_not_lock_each_other_out() {
    use henk_domain::allowlist::Platform;
    use henk_store::NewRun;

    let file = File::new();
    let store = std::sync::Arc::new(Candidate::open(&file.0).await.unwrap());
    let tasks: Vec<_> = (0..8)
        .map(|task| {
            let store = std::sync::Arc::clone(&store);
            tokio::spawn(async move {
                for n in 0..50 {
                    let id = format!("w{task}-{n}");
                    store
                        .create_run(&NewRun {
                            id: run_id(&id),
                            kind: RunKind::Review,
                            platform: Platform::GitHub,
                            repo: "o/r".into(),
                            target: n,
                            commit: None,
                            requester: None,
                            trigger: "test".into(),
                            link: format!("https://henk.example/runs/{id}"),
                        })
                        .await?;
                    store.event(&run_id(&id), "info", "started").await?;
                }
                Ok::<(), henk_store::StoreError>(())
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        store.count_runs(&RunFilter::default()).await.unwrap(),
        400,
        "every write landed"
    );
    // Three more writes, through the trait's other write shapes.
    let parent = run_id("w0-0");
    store
        .link_child(&parent, henk_store::Stage::Plan, &run_id("w1-1"))
        .await
        .unwrap();
    store
        .link_child(&parent, henk_store::Stage::Plan, &run_id("w2-2"))
        .await
        .unwrap();
    assert_eq!(store.children(&parent).await.unwrap().len(), 1, "an upsert");
    store
        .finish_run(&parent, RunStatus::Finished, Some("done"), None)
        .await
        .unwrap();
    assert_eq!(
        store.run(&parent).await.unwrap().unwrap().status,
        RunStatus::Finished
    );
}

/// `PostgreSQL`, when `HENK_TEST_DATABASE_URL` names a server. Every test gets
/// its own schema, dropped with the guard.
mod postgres {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const URL: &str = "HENK_TEST_DATABASE_URL";

    static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

    struct Schema {
        url: String,
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
                    if let Ok(client) = client(&base).await {
                        let _ = client
                            .batch_execute(&format!("DROP SCHEMA {name} CASCADE"))
                            .await;
                    }
                });
            })
            .join();
        }
    }

    async fn client(url: &str) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
        tokio::spawn(connection);
        Ok(client)
    }

    async fn schema() -> Schema {
        let base = std::env::var(URL).unwrap_or_else(|_| panic!("{URL} is not set"));
        let name = format!(
            "henk_golden_{}_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
            OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .unsigned_abs()
        );
        client(&base)
            .await
            .unwrap()
            .batch_execute(&format!("CREATE SCHEMA {name}"))
            .await
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{name}");
        Schema { url, name, base }
    }

    struct PgSeeder(String);

    impl Seeder for PgSeeder {
        async fn execute(&self, sql: &str) {
            client(&self.0)
                .await
                .unwrap()
                .batch_execute(sql)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn postgres_returns_what_the_goldens_say() {
        let schema = schema().await;
        // Connecting migrates; then the rows go in behind the store's back.
        let store = Candidate::connect(&schema.url).await.unwrap();
        PgSeeder(schema.url.clone()).execute(SEED).await;
        check("postgres", &report(&store).await);
        assert_eq!(
            scalar(&schema.url, "SELECT count(*) FROM seaql_migrations").await,
            4
        );
    }

    /// A schema the OLD store made (016, `henk_schema_migrations`), filled
    /// with the seed, opened with the candidate.
    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn postgres_016_is_adopted_and_returns_what_the_goldens_say() {
        let schema = schema().await;
        drop(PgStore::connect(&schema.url).await.unwrap());
        PgSeeder(schema.url.clone()).execute(SEED).await;
        assert_eq!(
            scalar(
                &schema.url,
                "SELECT max(version)::bigint FROM henk_schema_migrations"
            )
            .await,
            16
        );
        let store = Candidate::connect(&schema.url).await.unwrap();
        check("postgres", &report(&store).await);
        assert_eq!(
            scalar(&schema.url, "SELECT count(*) FROM seaql_migrations").await,
            4,
            "three adopted, the fourth step ran"
        );
        assert_eq!(
            scalar(
                &schema.url,
                "SELECT max(version)::bigint FROM henk_schema_migrations"
            )
            .await,
            16,
            "the old bookkeeping is left as it was"
        );
        // Again: nothing to adopt, nothing to run.
        drop(store);
        let store = Candidate::connect(&schema.url).await.unwrap();
        check("postgres", &report(&store).await);
        assert_eq!(
            scalar(&schema.url, "SELECT count(*) FROM seaql_migrations").await,
            4
        );
    }

    /// Two Henk processes starting together on one new database.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn two_processes_migrating_at_once_do_not_collide() {
        let schema = schema().await;
        let (a, b) = tokio::join!(
            Candidate::connect(&schema.url),
            Candidate::connect(&schema.url)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(
            scalar(&schema.url, "SELECT count(*) FROM seaql_migrations").await,
            4
        );
    }
}
