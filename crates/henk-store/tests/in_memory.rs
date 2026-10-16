//! `SqliteStore::in_memory` is a file under the temp directory while two
//! libraries share the database (#598), and it must not outlive the store.
//!
//! This is a test file of its own, with one test, so no other test of the
//! process makes in-memory stores while the files are counted.

#![allow(clippy::unwrap_used)]

use henk_domain::allowlist::Platform;
use henk_domain::run::{RunId, RunKind};
use henk_store::{NewRun, RunStore, SqliteStore};

/// How many files of in-memory stores this process has in the temp directory
/// (the database and its WAL files).
fn temporary_files() -> usize {
    let prefix = format!("henk-store-memory-{}-", std::process::id());
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .count()
}

#[tokio::test]
async fn an_in_memory_store_leaves_no_file_behind() {
    assert_eq!(temporary_files(), 0);
    {
        let store = SqliteStore::in_memory().unwrap();
        store
            .create_run(&NewRun {
                id: RunId::parse("r-1").unwrap(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "o/r".into(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "opened".into(),
                link: "https://henk.example/runs/r-1".into(),
            })
            .await
            .unwrap();
        assert!(temporary_files() > 0, "the store has a file while it lives");
    }
    assert_eq!(temporary_files(), 0);
}
