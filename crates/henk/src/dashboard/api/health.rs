//! What the service has, from configuration and the store: what the
//! health view shows. Nothing here starts a process or calls a model; that
//! is `henk doctor --probe`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_domain::run::RunKind;
use henk_domain::workspace::EnvLane;
use henk_store::{Page, RunFilter, RunRecord, RunStatus, Stage, StageState};

use super::types::{Health, HealthCheck};
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;
use crate::doctor::{Verdict, check_secrets};

/// `GET /health`.
pub async fn health(State(dashboard): State<Arc<Dashboard>>, _viewer: ApiViewer) -> Json<Health> {
    let checks = health_rows(&dashboard)
        .await
        .into_iter()
        .map(|(name, state, detail)| HealthCheck {
            name,
            state,
            detail,
        })
        .collect();
    Json(Health { checks })
}

/// What was checked, `ok`, `warn` or `fail`, and the detail.
async fn health_rows(dashboard: &Dashboard) -> Vec<(String, String, String)> {
    let settings = &dashboard.app.settings;
    let store = &dashboard.app.store;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let store_ok = store
        .list_runs(&RunFilter::default(), Page::new(1, 0))
        .await
        .map(|_| ());
    rows.push((
        "database".to_owned(),
        if store_ok.is_ok() { "ok" } else { "fail" }.to_owned(),
        store_ok
            .err()
            .map_or_else(|| settings.database.describe(), |e| e.to_string()),
    ));
    let stale = time::OffsetDateTime::now_utc() - crate::liveness::STALE_AFTER;
    let orphans = store.orphaned_runs(stale).await.map_or(0, |o| o.len());
    rows.push((
        "silent runs".to_owned(),
        if orphans == 0 { "ok" } else { "warn" }.to_owned(),
        format!("{orphans} running without a heartbeat; the next start closes them"),
    ));
    rows.push((
        "listeners".to_owned(),
        "ok".to_owned(),
        dashboard.listeners.join(", "),
    ));
    rows.push((
        "reviews".to_owned(),
        "ok".to_owned(),
        reviews_line(&dashboard.coordinator.slots()),
    ));
    rows.push(workspaces_row(dashboard).await);
    let git = crate::doctor::check_git(settings, None).await;
    rows.push(row(git));
    for check in check_secrets(settings) {
        let (verdict, text) = match check.verdict {
            Verdict::Ok(text) => ("ok", text),
            Verdict::Warn(text) => ("warn", text),
            Verdict::Fail(text) => ("fail", text),
        };
        rows.push((check.name, verdict.to_owned(), text));
    }
    rows.push((
        "models".to_owned(),
        "ok".to_owned(),
        settings
            .models
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
    ));
    rows.push((
        "MCP servers".to_owned(),
        "ok".to_owned(),
        settings.mcp.keys().cloned().collect::<Vec<_>>().join(", "),
    ));
    rows
}

/// A doctor check as a row.
fn row(check: crate::doctor::Check) -> (String, String, String) {
    let (state, text) = match check.verdict {
        Verdict::Ok(text) => ("ok", text),
        Verdict::Warn(text) => ("warn", text),
        Verdict::Fail(text) => ("fail", text),
    };
    (check.name, state.to_owned(), text)
}

/// How many reviews to look back at for workspaces.
const RECENT_REVIEWS: usize = 10;

/// Whether the reviews configured for workspaces got them (#254): of the
/// last ended ones that reached their checkout, how many had their commit
/// checked out, and why the newest that did not went without. A failing
/// checkout leaves a review that still completes, so nothing else would
/// say so.
async fn workspaces_row(dashboard: &Dashboard) -> (String, String, String) {
    let settings = &dashboard.app.settings;
    let name = "workspaces".to_owned();
    if !settings
        .workspace
        .named()
        .any(|(_, profile)| profile.serves(EnvLane::Review))
    {
        let text = "reviews run without workspaces: no profile has review = true";
        return (name, "ok".to_owned(), text.to_owned());
    }
    let store = &dashboard.app.store;
    let filter = RunFilter {
        kind: Some(RunKind::Review),
        ..RunFilter::default()
    };
    let Ok(runs) = store.list_runs(&filter, Page::new(Page::MAX, 0)).await else {
        return (
            name,
            "warn".to_owned(),
            "could not read the reviews".to_owned(),
        );
    };
    let ended: Vec<&RunRecord> = runs
        .iter()
        .filter(|run| run.status != RunStatus::Running && run.status != RunStatus::Superseded)
        .filter(|run| {
            settings
                .workspace
                .profile_for(&run.repo)
                .serves(EnvLane::Review)
        })
        .collect();
    let ids: Vec<_> = ended.iter().map(|run| run.id.clone()).collect();
    let stages = store.stages_of(&ids).await.unwrap_or_default();
    let checkout = |run: &RunRecord, wanted: StageState| {
        stages.iter().any(|(id, stage, state)| {
            *id == run.id && *stage == Stage::Checkout && *state == wanted
        })
    };
    // Only a review whose checkout was tried and went wrong went without:
    // one with nothing to review skips it, one cancelled in the queue or
    // failed before its lanes never gets there, and one that ended while
    // its checkout was running has the stage failed for it, without the
    // warning a failed checkout leaves.
    let mut counted = 0;
    let mut had = 0;
    let mut why = None;
    for run in ended {
        if counted == RECENT_REVIEWS {
            break;
        }
        if checkout(run, StageState::Done) {
            counted += 1;
            had += 1;
        } else if checkout(run, StageState::Failed)
            && let Some(warning) = why_without(dashboard, run).await
        {
            counted += 1;
            why.get_or_insert(warning);
        }
    }
    if counted == 0 {
        let text = "no review with workspaces has reached its checkout yet";
        return (name, "ok".to_owned(), text.to_owned());
    }
    let line = format!("{had} of the last {counted} reviews had one");
    match why {
        None => (name, "ok".to_owned(), line),
        Some(why) => (name, "warn".to_owned(), format!("{line} ({why})")),
    }
}

/// Why `run` went without workspaces, from the warning its failed checkout
/// left; none when the checkout did not fail on its own.
async fn why_without(dashboard: &Dashboard, run: &RunRecord) -> Option<String> {
    const CHECKOUT: &str = "could not be checked out: ";
    let events = dashboard
        .app
        .store
        .events(&run.id)
        .await
        .unwrap_or_default();
    events
        .iter()
        .rev()
        .find(|event| event.level == "warn" && event.message.starts_with("review workspaces:"))
        .map(|event| match event.message.split_once(CHECKOUT) {
            Some((_, error)) => format!("checkout failed: {error}"),
            None => event
                .message
                .trim_start_matches("review workspaces: ")
                .to_owned(),
        })
}

/// The review slots in words: `2 running, 1 waiting (limit 2)`.
fn reviews_line(slots: &crate::coordinator::Slots) -> String {
    format!(
        "{} running, {} waiting (limit {})",
        slots.in_use,
        slots.waiting.len(),
        slots.limit
    )
}
