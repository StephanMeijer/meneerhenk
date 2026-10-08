//! What the service has, from configuration and the store: what the
//! health view shows. Nothing here starts a process or calls a model; that
//! is `henk doctor --probe`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use henk_store::{Page, RunFilter};

use super::types::{Health, HealthCheck};
use crate::app::App;
use crate::coordinator::Coordinator;
use crate::dashboard::Dashboard;
use crate::dashboard::auth::ApiViewer;
use crate::doctor::{Verdict, check_secrets};

/// `GET /health`.
pub async fn health(State(dashboard): State<Arc<Dashboard>>, _viewer: ApiViewer) -> Json<Health> {
    Json(health_of(&dashboard.app, &dashboard.coordinator, &dashboard.listeners).await)
}

/// What the service has: `GET /health` and the MCP server's `health`.
pub(crate) async fn health_of(app: &App, coordinator: &Coordinator, listeners: &[&str]) -> Health {
    let checks = health_rows(app, coordinator, listeners)
        .await
        .into_iter()
        .map(|(name, state, detail)| HealthCheck {
            name,
            state,
            detail,
        })
        .collect();
    Health { checks }
}

/// What was checked, `ok`, `warn` or `fail`, and the detail.
async fn health_rows(
    app: &App,
    coordinator: &Coordinator,
    listeners: &[&str],
) -> Vec<(String, String, String)> {
    let settings = &app.settings;
    let store = &app.store;
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
        listeners.join(", "),
    ));
    rows.push((
        "reviews".to_owned(),
        "ok".to_owned(),
        reviews_line(&coordinator.slots()),
    ));
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

/// The review slots in words: `2 running, 1 waiting (limit 2)`.
fn reviews_line(slots: &crate::coordinator::Slots) -> String {
    format!(
        "{} running, {} waiting (limit {})",
        slots.in_use,
        slots.waiting.len(),
        slots.limit
    )
}
