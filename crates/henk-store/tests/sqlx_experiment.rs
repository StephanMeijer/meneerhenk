//! Scratch experiments on sqlx + SQLite busy behaviour (not part of the spike's result).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Instant;

use henk_domain::allowlist::Platform;
use henk_domain::run::{RunId, RunKind};
use henk_store::{Candidate, NewRun, Page, RunFilter, RunStore};

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

async fn run(label: &str, events: bool, reader: bool, list_only: bool) -> bool {
    let path = std::env::temp_dir().join(format!("henk-exp-{}-{label}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(Candidate::open(&path).await.unwrap());
    let started = Instant::now();
    let mut tasks = Vec::new();
    for task in 0..8_i64 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            for n in 0..50_i64 {
                let op = Instant::now();
                let id = format!("w{task}-{n:03}");
                store
                    .create_run(&new_run(&id))
                    .await
                    .map_err(|e| format!("task {task} create {n} after {:?}: {e}", op.elapsed()))?;
                if events {
                    store
                        .event(&RunId::parse(id.as_str()).unwrap(), "info", "x")
                        .await
                        .map_err(|e| format!("task {task} event {n} after {:?}: {e}", op.elapsed()))?;
                }
            }
            Ok::<(), String>(())
        }));
    }
    if reader {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            for _ in 0..100 {
                if list_only {
                    store
                        .list_runs(&RunFilter::default(), Page::new(20, 0))
                        .await
                        .map_err(|e| format!("list: {e}"))?;
                } else {
                    store
                        .count_runs(&RunFilter::default())
                        .await
                        .map_err(|e| format!("count: {e}"))?;
                }
            }
            Ok::<(), String>(())
        }));
    }
    let mut ok = true;
    for task in tasks {
        if let Err(e) = task.await.unwrap() {
            eprintln!("{label}: {e}");
            ok = false;
        }
    }
    eprintln!("{label}: ok={ok} in {:?}", started.elapsed());
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    ok
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn experiments() {
    let mut results = Vec::new();
    for round in 0..3 {
        for (label, events, reader, list_only) in [
            ("create-only", false, false, false),
            ("create+event", true, false, false),
            ("create+reader-count", false, true, false),
            ("create+reader-list", false, true, true),
        ] {
            let ok = run(&format!("{label}-{round}"), events, reader, list_only).await;
            results.push((label, ok));
        }
    }
    for (label, ok) in &results {
        eprintln!("RESULT {label} {ok}");
    }
}
