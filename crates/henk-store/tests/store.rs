//! The store as callers hold it, on the cases that depend on how it is
//! opened and shared (#605): lazy first use, file names, a number the
//! database cannot hold, and writers that share one database.
//!
//! `conformance.rs` covers what each method does; the golden fixtures cover
//! what the listings return. `PostgreSQL` has the same writers test,
//! `#[ignore]`d, for a server named by `HENK_TEST_DATABASE_URL`.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use henk_domain::allowlist::Platform;
use henk_domain::run::{RunId, RunKind};
use henk_store::{
    NewRun, RoundRecord, RunFilter, RunStatus, RunStore, SessionRole, SessionStatus, SqliteStore,
    Stage, StageState, StageWrite, StoreError, TokenUsage, TranscriptRecord,
};

fn new_run(id: &str, target: u64) -> NewRun {
    NewRun {
        id: RunId::parse(id).unwrap(),
        kind: RunKind::Review,
        platform: Platform::GitHub,
        repo: "o/r".into(),
        target,
        commit: Some("abc".into()),
        requester: None,
        trigger: "opened".into(),
        link: format!("https://henk.example/runs/{id}"),
    }
}

/// Numbers this process's database files, so no two tests share one.
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

/// A database file under the temp directory, removed with the guard. The
/// directory name carries a space and a `?`, which a URL would have broken.
struct File(PathBuf);

impl File {
    fn new() -> Self {
        let n = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("henk store? {}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.join("runs #1.db"))
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn sqlite_steps(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM seaql_migrations", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[tokio::test]
async fn a_file_name_with_a_space_a_question_mark_and_a_hash_opens() {
    let file = File::new();
    let store = SqliteStore::open(&file.0).unwrap();
    store.create_run(&new_run("r-1", 7)).await.unwrap();
    assert!(store.run(&new_run("r-1", 7).id).await.unwrap().is_some());
    assert!(file.0.exists());
}

#[tokio::test]
async fn a_target_above_i64_max_is_refused_and_a_filter_for_one_matches_nothing() {
    let store = SqliteStore::in_memory().unwrap();
    let too_big = u64::try_from(i64::MAX).unwrap() + 1;

    let error = store
        .create_run(&new_run("r-big", too_big))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Corrupt { .. }), "{error:?}");

    store.create_run(&new_run("r-1", 7)).await.unwrap();
    let filter = RunFilter {
        target: Some(too_big),
        ..RunFilter::default()
    };
    assert_eq!(store.count_runs(&filter).await.unwrap(), 0);
}

#[tokio::test]
async fn the_first_calls_at_once_open_and_migrate_the_database_once() {
    let file = File::new();
    let store = Arc::new(SqliteStore::open(&file.0).unwrap());

    let calls: Vec<_> = (0..16)
        .map(|n| {
            let store = store.clone();
            tokio::spawn(async move { store.create_run(&new_run(&format!("r-{n}"), n)).await })
        })
        .collect();
    for call in calls {
        call.await.unwrap().unwrap();
    }

    assert_eq!(sqlite_steps(&file.0), 5, "every step recorded once");
    assert_eq!(store.count_runs(&RunFilter::default()).await.unwrap(), 16);
}

#[tokio::test]
async fn opening_a_path_that_cannot_be_created_fails_at_once() {
    // A path that cannot be created: the old half fails at once, as always.
    let error = SqliteStore::open(Path::new("/nonexistent-dir/henk.db")).unwrap_err();
    assert!(matches!(error, StoreError::Database(_)), "{error:?}");
}

/// Eight writers, each making runs and writing their stages, heartbeats and
/// ends through one store: nothing fails with "database is locked".
async fn eight_writers(store: Arc<dyn RunStore>) {
    let writers: Vec<_> = (0..8_u64)
        .map(|writer| {
            let store = store.clone();
            tokio::spawn(async move {
                for n in 0..20_u64 {
                    let run = new_run(&format!("w{writer}-{n}"), n);
                    store.create_run(&run).await.unwrap();
                    store.heartbeat(&run.id).await.unwrap();
                    store
                        .stage(
                            &run.id,
                            &StageWrite::now(Stage::Review, StageState::Running, ""),
                        )
                        .await
                        .unwrap();
                    store
                        .start_session(&run.id, "reviewer", SessionRole::Reviewer, "m")
                        .await
                        .unwrap();
                    store.event(&run.id, "info", "started").await.unwrap();
                    store.joined(&run.id, "comment").await.unwrap();
                    store
                        .record_transcript(&run.id, &transcript("reviewer", "{}"))
                        .await
                        .unwrap();
                    store
                        .finish_session(
                            &run.id,
                            "reviewer",
                            SessionStatus::Finished,
                            3,
                            TokenUsage::default(),
                            None,
                        )
                        .await
                        .unwrap();
                    store
                        .finish_run(&run.id, RunStatus::Finished, Some("done"), None)
                        .await
                        .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.await.unwrap();
    }
    assert_eq!(store.count_runs(&RunFilter::default()).await.unwrap(), 160);
    let run = RunId::parse("w3-7").unwrap();
    assert_eq!(store.sessions(&run).await.unwrap().len(), 1);
    assert_eq!(store.events(&run).await.unwrap().len(), 1);
    assert_eq!(store.transcripts(&run).await.unwrap().len(), 1);
}

fn transcript(session: &str, body: &str) -> TranscriptRecord {
    TranscriptRecord {
        at: String::new(),
        session: session.into(),
        model: "m".into(),
        stop: "EndTurn".into(),
        turns: 1,
        bytes: u64::try_from(body.len()).unwrap(),
        body: body.into(),
    }
}

fn round(started_at: &str) -> RoundRecord {
    RoundRecord {
        session: "reviewer".into(),
        role: SessionRole::Reviewer,
        round: 1,
        started_at: started_at.into(),
        ended_at: None,
        first_turn: 1,
        turns: 0,
        handover: String::new(),
        handover_len: 0,
    }
}

#[tokio::test]
async fn a_time_that_is_not_rfc_3339_is_refused_not_stored() {
    let store = SqliteStore::in_memory().unwrap();
    store.create_run(&new_run("r-1", 7)).await.unwrap();
    let run = RunId::parse("r-1").unwrap();

    let mut late = transcript("reviewer", "{}");
    late.at = "yesterday".into();
    let error = store.record_transcript(&run, &late).await.unwrap_err();
    assert!(matches!(error, StoreError::Corrupt { .. }), "{error:?}");
    let error = store.start_round(&run, &round("noon")).await.unwrap_err();
    assert!(matches!(error, StoreError::Corrupt { .. }), "{error:?}");

    // An empty time means now, and a real one is kept.
    store
        .record_transcript(&run, &transcript("a", "{}"))
        .await
        .unwrap();
    store
        .start_round(&run, &round("2026-10-01T10:00:00.4Z"))
        .await
        .unwrap();
    let rounds = store.rounds(&run).await.unwrap();
    assert_eq!(rounds[0].started_at, "2026-10-01T10:00:00.4Z");
    assert_eq!(store.transcripts(&run).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_writers_share_one_sqlite_file() {
    let file = File::new();
    eight_writers(Arc::new(SqliteStore::open(&file.0).unwrap())).await;
}

mod postgres {
    use henk_store::PgStore;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn eight_writers_share_one_postgres_schema() {
        let base = std::env::var("HENK_TEST_DATABASE_URL").expect("HENK_TEST_DATABASE_URL");
        let name = format!(
            "henk_store_{}_{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        );
        let (client, connection) = tokio_postgres::connect(&base, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        client
            .batch_execute(&format!("CREATE SCHEMA {name}"))
            .await
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{name}");

        eight_writers(Arc::new(PgStore::connect(&url).await.unwrap())).await;

        client
            .batch_execute(&format!("DROP SCHEMA {name} CASCADE"))
            .await
            .unwrap();
    }
}
