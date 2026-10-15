//! The sqlx candidate (spike #601) outside the goldens: many writers on one
//! SQLite file, the in-memory database, and the query plan of the run
//! listing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Instant;

use henk_domain::allowlist::Platform;
use henk_domain::run::{RunId, RunKind};
use henk_store::{Candidate, NewRun, Page, RunFilter, RunStore};
use sqlx::{AnyConnection, AssertSqlSafe, Connection as _, Row as _};

const TASKS: usize = 8;
const WRITES: usize = 50;

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

/// A database file under the temp directory, removed with the guard.
struct File(std::path::PathBuf);

impl File {
    fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(format!("henk-sqlx-{}-{name}.db", std::process::id())))
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

/// 8 tasks, 50 writes each (a run, and an event on it), against one file,
/// while a reader lists runs: no "database is locked", no error at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_tasks_write_to_one_sqlite_file() {
    let file = File::new("writers");
    let store = Arc::new(Candidate::open(&file.0).await.unwrap());
    let started = Instant::now();
    let mut tasks = Vec::new();
    for task in 0..TASKS {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            for write in 0..WRITES {
                let id = format!("w{task}-{write:03}");
                let op = Instant::now();
                store
                    .create_run(&new_run(&id))
                    .await
                    .map_err(|e| format!("create_run {id} after {:?}: {e}", op.elapsed()))?;
                store
                    .event(&RunId::parse(id.as_str()).unwrap(), "info", "written")
                    .await
                    .map_err(|e| format!("event {id}: {e}"))?;
            }
            Ok::<(), String>(())
        }));
    }
    let reader = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            for _ in 0..100 {
                store
                    .list_runs(&RunFilter::default(), Page::new(20, 0))
                    .await
                    .map_err(|e| format!("list_runs: {e}"))?;
                store
                    .count_runs(&RunFilter::default())
                    .await
                    .map_err(|e| format!("count_runs: {e}"))?;
            }
            Ok::<(), String>(())
        })
    };
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    reader.await.unwrap().unwrap();
    eprintln!(
        "{} writes in {:?}",
        TASKS * WRITES * 2,
        started.elapsed()
    );
    assert_eq!(
        store.count_runs(&RunFilter::default()).await.unwrap(),
        (TASKS * WRITES) as u64
    );
    let run = RunId::parse("w3-017").unwrap();
    assert_eq!(store.events(&run).await.unwrap().len(), 1);
}

/// The pool opens SQLite as the old store did: WAL, foreign keys on, and a
/// busy timeout.
#[tokio::test]
async fn a_sqlite_file_is_wal_with_foreign_keys_on() {
    let file = File::new("pragmas");
    let store = Candidate::open(&file.0).await.unwrap();
    // A run row is needed for the event's foreign key to be satisfied: an
    // event for no run is refused.
    let error = store
        .event(&RunId::parse("r-none").unwrap(), "info", "x")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
    let mut connection = AnyConnection::connect(&format!(
        "sqlite://{}?mode=rwc",
        file.0.display()
    ))
    .await
    .unwrap();
    let mode: String = sqlx::query("PRAGMA journal_mode")
        .fetch_one(&mut connection)
        .await
        .unwrap()
        .get(0);
    assert_eq!(mode, "wal");
}

/// `Candidate::in_memory` is one database for every call, however the pool
/// hands out connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_in_memory_database_is_one_database() {
    let store = Arc::new(Candidate::in_memory().await.unwrap());
    let mut tasks = Vec::new();
    for task in 0..4 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            for write in 0..10 {
                store
                    .create_run(&new_run(&format!("m{task}-{write}")))
                    .await
                    .unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(store.count_runs(&RunFilter::default()).await.unwrap(), 40);
}

/// The run listing walks the expression index of step 2 instead of sorting
/// (the check `sqlite.rs` had for `runs_started_instant`).
#[tokio::test]
async fn the_run_listing_walks_the_instant_index_instead_of_sorting() {
    let file = File::new("plan");
    drop(Candidate::open(&file.0).await.unwrap());
    let mut connection = AnyConnection::connect(&format!(
        "sqlite://{}?mode=rwc",
        file.0.display()
    ))
    .await
    .unwrap();
    let at = "substr(started_at, 1, 19) || substr(CASE WHEN substr(started_at, 20, 1) = '.' THEN substr(started_at, 21, length(started_at) - 21) ELSE '' END || '000000000', 1, 9)";
    let plan: Vec<String> = sqlx::query(AssertSqlSafe(format!(
        "EXPLAIN QUERY PLAN SELECT id FROM runs
         WHERE ($1 IS NULL OR kind = $1) ORDER BY {at} DESC, id DESC LIMIT 10 OFFSET 0"
    )))
    .bind(None::<String>)
    .fetch_all(&mut connection)
    .await
    .unwrap()
    .iter()
    .map(|row| row.get::<String, _>(3))
    .collect();
    assert!(
        plan.iter()
            .any(|step| step.contains("USING INDEX runs_started_instant")),
        "{plan:?}"
    );
    assert!(
        !plan.iter().any(|step| step.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}

/// The same writers against `PostgreSQL`, through the pool of 8.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs HENK_TEST_DATABASE_URL"]
async fn eight_tasks_write_to_postgres() {
    let base = std::env::var("HENK_TEST_DATABASE_URL").expect("HENK_TEST_DATABASE_URL");
    let name = format!("henk_sqlx_writers_{}", std::process::id());
    let admin = {
        let (client, connection) = tokio_postgres::connect(&base, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        client
    };
    admin
        .batch_execute(&format!("CREATE SCHEMA {name}"))
        .await
        .unwrap();
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-c%20search_path%3D{name}");
    let result = async {
        let store = Arc::new(Candidate::connect(&url).await.unwrap());
        let mut tasks = Vec::new();
        for task in 0..TASKS {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                for write in 0..WRITES {
                    store
                        .create_run(&new_run(&format!("w{task}-{write:03}")))
                        .await
                        .unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(
            store.count_runs(&RunFilter::default()).await.unwrap(),
            (TASKS * WRITES) as u64
        );
    }
    .await;
    admin
        .batch_execute(&format!("DROP SCHEMA {name} CASCADE"))
        .await
        .unwrap();
    result
}
