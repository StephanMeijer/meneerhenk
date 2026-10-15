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
use sqlx::{AnyConnection, AssertSqlSafe, Connection as _, Row as _};
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
    check_from(database, report, &[]);
}

/// [`check`], but a file named in `elsewhere` is compared with the same file
/// of another database's directory: `(name, database)`. For the one place
/// the two stores differ today (see the header) where the store under test
/// does what the other one does.
fn check_from(database: &str, report: &[(&str, String)], elsewhere: &[(&str, &str)]) {
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let dir = golden.join(database);
    let bless = std::env::var_os("HENK_BLESS").is_some();
    if bless {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let mut stale = Vec::new();
    for (name, text) in report {
        let from = elsewhere
            .iter()
            .find(|(file, _)| file == name)
            .map_or(dir.clone(), |(_, other)| golden.join(other));
        let path = from.join(format!("{name}.txt"));
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

/// A second connection to the file, of sqlx's own, behind the store's back.
async fn sqlite_connection(path: &Path) -> AnyConnection {
    sqlx::any::install_default_drivers();
    AnyConnection::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap()
}

struct SqliteSeeder(PathBuf);

impl Seeder for SqliteSeeder {
    async fn execute(&self, sql: &str) {
        sqlx::raw_sql(AssertSqlSafe(sql.to_owned()))
            .execute(&mut sqlite_connection(&self.0).await)
            .await
            .unwrap();
    }
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
    check_from("sqlite", &report(&store).await, SQLITE_INBOUND_BY_INSTANT);
}

/// This store lists inbound events by instant on SQLite too, as PostgreSQL
/// does, so that file is compared with the PostgreSQL golden (the header
/// says why the two differ today).
const SQLITE_INBOUND_BY_INSTANT: &[(&str, &str)] = &[("inbound_events", "postgres")];

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
    let mut connection = sqlite_connection(&file.0).await;
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
        let rows: i64 = sqlx::query(AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(&mut connection)
            .await
            .unwrap()
            .get(0);
        assert!(rows > 0, "the seed puts nothing in {table}");
    }
}

/// The 20 SQLite migrations of the previous store, in order: what a database
/// at `PRAGMA user_version = 20` went through.
const LEGACY_SQLITE: [&str; 20] = [
    include_str!("../migrations/sqlite/001_initial.sql"),
    include_str!("../migrations/sqlite/002_inbound_events.sql"),
    include_str!("../migrations/sqlite/003_run_liveness.sql"),
    include_str!("../migrations/sqlite/004_dashboard.sql"),
    include_str!("../migrations/sqlite/005_prune_events.sql"),
    include_str!("../migrations/sqlite/006_event_requester.sql"),
    include_str!("../migrations/sqlite/007_tool_calls.sql"),
    include_str!("../migrations/sqlite/008_transcripts.sql"),
    include_str!("../migrations/sqlite/009_drafts.sql"),
    include_str!("../migrations/sqlite/010_superseded_by.sql"),
    include_str!("../migrations/sqlite/011_stages.sql"),
    include_str!("../migrations/sqlite/012_stats.sql"),
    include_str!("../migrations/sqlite/013_run_instant.sql"),
    include_str!("../migrations/sqlite/014_loop.sql"),
    include_str!("../migrations/sqlite/015_lane_cache.sql"),
    include_str!("../migrations/sqlite/016_sessions.sql"),
    include_str!("../migrations/sqlite/017_rounds.sql"),
    include_str!("../migrations/sqlite/018_reasoning_tokens.sql"),
    include_str!("../migrations/sqlite/019_tool_names.sql"),
    include_str!("../migrations/sqlite/020_run_children.sql"),
];

/// A database file the previous store would have left: its 20 migrations
/// run in order and `user_version` set to 20, as rusqlite_migration does.
async fn legacy_sqlite(path: &Path) {
    let mut connection = sqlite_connection(path).await;
    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&mut connection)
        .await
        .unwrap();
    for sql in LEGACY_SQLITE {
        sqlx::raw_sql(sql).execute(&mut connection).await.unwrap();
    }
    sqlx::query("PRAGMA user_version = 20")
        .execute(&mut connection)
        .await
        .unwrap();
}

/// The versions sqlx's migrator recorded in `_sqlx_migrations`, and how long
/// each took (-1 for a step it recorded without running).
async fn recorded_steps(path: &Path) -> Vec<(i64, i64)> {
    sqlx::query("SELECT version, execution_time FROM _sqlx_migrations ORDER BY version")
        .fetch_all(&mut sqlite_connection(path).await)
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

#[tokio::test]
async fn sqlite_adopts_a_database_the_previous_migrations_made() {
    let file = File::new();
    legacy_sqlite(&file.0).await;
    SqliteSeeder(file.0.clone()).execute(SEED).await;
    // Opening adopts it: the steps it already has are recorded, not run.
    let store = Candidate::open(&file.0).await.unwrap();
    check_from("sqlite", &report(&store).await, SQLITE_INBOUND_BY_INSTANT);
    let steps = recorded_steps(&file.0).await;
    assert_eq!(steps.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(
        steps.iter().all(|s| s.1 == -1),
        "recorded without running: {steps:?}"
    );
    // The old bookkeeping is left as it was.
    let version: i64 = sqlx::query("PRAGMA user_version")
        .fetch_one(&mut sqlite_connection(&file.0).await)
        .await
        .unwrap()
        .get(0);
    assert_eq!(version, 20);
    // And opening it again changes nothing.
    drop(store);
    let again = Candidate::open(&file.0).await.unwrap();
    check_from("sqlite", &report(&again).await, SQLITE_INBOUND_BY_INSTANT);
    assert_eq!(recorded_steps(&file.0).await, steps);
}

/// The database `SqliteStore` made at schema 020 and filled with the seed
/// (`HENK_FIXTURE_SQLITE_020` names it; it is copied first).
#[tokio::test]
#[ignore = "needs HENK_FIXTURE_SQLITE_020"]
async fn sqlite_adopts_the_fixture_the_previous_store_made() {
    let fixture = std::env::var("HENK_FIXTURE_SQLITE_020").expect("HENK_FIXTURE_SQLITE_020");
    let file = File::new();
    std::fs::copy(fixture, &file.0).unwrap();
    let store = Candidate::open(&file.0).await.unwrap();
    check_from("sqlite", &report(&store).await, SQLITE_INBOUND_BY_INSTANT);
    let steps = recorded_steps(&file.0).await;
    assert_eq!(steps, [(1, -1), (2, -1), (3, -1)]);
}

/// A database older than the one the steps restate is refused with a
/// message, not half-adopted.
#[tokio::test]
async fn sqlite_refuses_a_database_at_another_version() {
    let file = File::new();
    legacy_sqlite(&file.0).await;
    sqlx::query("PRAGMA user_version = 19")
        .execute(&mut sqlite_connection(&file.0).await)
        .await
        .unwrap();
    let error = Candidate::open(&file.0).await.unwrap_err();
    assert!(error.to_string().contains("schema version 19"), "{error}");
}

/// What the tables look like: each one's columns and its indexes' names.
async fn sqlite_shape(path: &Path) -> Vec<String> {
    let mut connection = sqlite_connection(path).await;
    let tables: Vec<String> = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table'
         AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '\\_sqlx%' ESCAPE '\\' ORDER BY name",
    )
    .fetch_all(&mut connection)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get(0))
    .collect();
    let mut shape = Vec::new();
    for table in tables {
        let columns = sqlx::query(AssertSqlSafe(format!(
            "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info('{table}') ORDER BY cid"
        )))
        .fetch_all(&mut connection)
        .await
        .unwrap();
        for row in columns {
            shape.push(format!(
                "{table}.{} {} notnull={} default={:?} pk={}",
                row.get::<String, _>(0),
                row.get::<String, _>(1),
                row.get::<i64, _>(2),
                row.get::<Option<String>, _>(3),
                row.get::<i64, _>(4),
            ));
        }
        let indexes = sqlx::query(AssertSqlSafe(format!(
            "SELECT name FROM pragma_index_list('{table}') WHERE origin = 'c' ORDER BY name"
        )))
        .fetch_all(&mut connection)
        .await
        .unwrap();
        for row in indexes {
            shape.push(format!("{table} index {}", row.get::<String, _>(0)));
        }
    }
    shape
}

/// A database made fresh has the schema of one the 20 migrations made: the
/// three steps restate the history, they do not change it.
#[tokio::test]
async fn the_steps_make_the_schema_the_20_migrations_make() {
    let (old, new) = (File::new(), File::new());
    legacy_sqlite(&old.0).await;
    drop(Candidate::open(&new.0).await.unwrap());
    let (old, new) = (sqlite_shape(&old.0).await, sqlite_shape(&new.0).await);
    assert!(old.len() > 100, "{}", old.len());
    assert_eq!(old, new);
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
    }

    /// A query on the schema's own connection, as text.
    async fn texts(url: &str, sql: &str) -> Vec<String> {
        client(url)
            .await
            .unwrap()
            .query(sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect()
    }

    /// The schema the previous store makes (`PgStore::connect` migrates
    /// 001 to 016 and records them in `henk_schema_migrations`) is adopted:
    /// recognised, not rebuilt, rows kept, and what it reads is unchanged.
    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn postgres_adopts_a_schema_the_previous_store_made() {
        let schema = schema().await;
        let old = PgStore::connect(&schema.url).await.unwrap();
        PgSeeder(schema.url.clone()).execute(SEED).await;
        // The oids of the tables: a rebuilt table has a new one.
        let oids = "SELECT string_agg(relname || ':' || oid::text, ',' ORDER BY relname)
                    FROM pg_class WHERE relnamespace = current_schema()::regnamespace
                    AND relkind = 'r' AND relname <> '_sqlx_migrations'";
        let before = texts(&schema.url, oids).await;
        drop(old);

        let store = Candidate::connect(&schema.url).await.unwrap();
        check("postgres", &report(&store).await);
        assert_eq!(texts(&schema.url, oids).await, before, "no table rebuilt");
        // sqlx recorded steps 1 and 3 (PostgreSQL has no step 2) without
        // running them, and the old table is left alone at 16.
        let steps = texts(
            &schema.url,
            "SELECT string_agg(version || ':' || execution_time, ',' ORDER BY version)
             FROM _sqlx_migrations",
        )
        .await;
        assert_eq!(steps, ["1:-1,3:-1"]);
        let old = texts(
            &schema.url,
            "SELECT max(version)::text FROM henk_schema_migrations",
        )
        .await;
        assert_eq!(old, ["16"]);
        // Connecting again changes nothing.
        drop(store);
        let again = Candidate::connect(&schema.url).await.unwrap();
        check("postgres", &report(&again).await);
    }

    /// What the tables look like: columns with their types, defaults and
    /// nullability, and the definition of each index, without the schema.
    async fn shape(url: &str, name: &str) -> Vec<String> {
        let mut shape = texts(
            url,
            "SELECT table_name || '.' || column_name || ' ' || data_type || ' '
                    || is_nullable || ' ' || coalesce(column_default, '-') || ' ' || is_identity
             FROM information_schema.columns
             WHERE table_schema = current_schema() AND table_name NOT IN ('_sqlx_migrations', 'henk_schema_migrations')
             ORDER BY table_name, ordinal_position",
        )
        .await;
        shape.extend(
            texts(
                url,
                "SELECT replace(indexdef, current_schema() || '.', '') FROM pg_indexes
                 WHERE schemaname = current_schema() AND tablename NOT IN ('_sqlx_migrations', 'henk_schema_migrations')
                 ORDER BY indexname",
            )
            .await,
        );
        assert!(shape.len() > 100, "{name}: {}", shape.len());
        shape
    }

    /// A schema made fresh has what one the 16 migrations made has.
    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn the_steps_make_the_schema_the_16_migrations_make() {
        let (old, new) = (schema().await, schema().await);
        drop(PgStore::connect(&old.url).await.unwrap());
        drop(Candidate::connect(&new.url).await.unwrap());
        let (old, new) = (shape(&old.url, "old").await, shape(&new.url, "new").await);
        // One difference is known: the previous migrations renamed `lanes`
        // to `sessions`, and PostgreSQL keeps the primary key's index name.
        let old: Vec<String> = old
            .iter()
            .map(|line| line.replace("lanes_pkey", "sessions_pkey"))
            .collect();
        let only = |a: &[String], b: &[String]| -> Vec<String> {
            a.iter().filter(|line| !b.contains(line)).cloned().collect()
        };
        assert_eq!(only(&old, &new), Vec::<String>::new(), "only in the old");
        assert_eq!(only(&new, &old), Vec::<String>::new(), "only in the new");
        assert_eq!(old.len(), new.len());
    }
}
