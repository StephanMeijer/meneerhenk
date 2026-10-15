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
    rows.extend(sandbox_host_rows(&app.workspace_provider.sandbox_hosts()));
    rows.push(workspaces_row(app).await);
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
        "endpoints".to_owned(),
        "ok".to_owned(),
        settings
            .endpoints
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
    ));
    rows.push((
        "models".to_owned(),
        "ok".to_owned(),
        settings
            .models
            .keys()
            .map(|id| format!("{id}: {}", settings.describe_model(id)))
            .collect::<Vec<_>>()
            .join("; "),
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
async fn workspaces_row(app: &App) -> (String, String, String) {
    let settings = &app.settings;
    let name = "workspaces".to_owned();
    if !settings
        .workspace
        .named()
        .any(|(_, profile)| profile.serves(EnvLane::Review))
    {
        let text = "reviews run without workspaces: no profile has review = true";
        return (name, "ok".to_owned(), text.to_owned());
    }
    let store = &app.store;
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
            && let Some(warning) = why_without(app, run).await
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
async fn why_without(app: &App, run: &RunRecord) -> Option<String> {
    const CHECKOUT: &str = "could not be checked out: ";
    let events = app.store.events(&run.id).await.unwrap_or_default();
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

/// One row per sandbox host (#403): `1 of 2 in use`, or `3 in use` with no
/// limit, a full host a warning while runs wait for one.
fn sandbox_host_rows(now: &crate::workspace::SandboxHosts) -> Vec<(String, String, String)> {
    now.hosts
        .iter()
        .map(|host| {
            let full = host.limit.is_some_and(|limit| host.in_use >= limit);
            let state = if full && now.waiting > 0 {
                "warn"
            } else {
                "ok"
            };
            let used = match host.limit {
                Some(limit) => format!("{} of {limit} in use", host.in_use),
                None => format!("{} in use", host.in_use),
            };
            let text = if now.waiting > 0 {
                format!("{used}, {} waiting for a sandbox host", now.waiting)
            } else {
                used
            };
            (format!("sandbox host {}", host.id), state.to_owned(), text)
        })
        .collect()
}

/// The slots in words: `2 running, 1 waiting (limit 2)`, and how many of
/// the waiting are rebases, which start first (#580): `2 running, 3
/// waiting, 1 rebase first (limit 2)`.
fn reviews_line(slots: &crate::coordinator::Slots) -> String {
    let rebases = slots
        .waiting
        .iter()
        .filter(|w| w.priority == henk_domain::queue::Priority::Rebase)
        .count();
    let first = match rebases {
        0 => String::new(),
        1 => ", 1 rebase first".to_owned(),
        n => format!(", {n} rebases first"),
    };
    format!(
        "{} running, {} waiting{first} (limit {})",
        slots.in_use,
        slots.waiting.len(),
        slots.limit
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::workspace::{HostSlots, SandboxHosts};

    #[test]
    fn the_reviews_row_counts_the_rebases_that_start_first() {
        use henk_domain::queue::Priority;
        use henk_domain::run::{RunId, RunKind};

        use crate::coordinator::{Reason, Slots, Waiting};

        let waiting = |priority, kind| Waiting {
            run: RunId::parse("r-20261015-00000001").unwrap(),
            kind,
            priority,
            platform: henk_domain::allowlist::Platform::GitHub,
            repo: "o/r".to_owned(),
            number: 7,
            commit: None,
            trigger: "asked".to_owned(),
            requester: None,
            since: time::OffsetDateTime::UNIX_EPOCH,
            position: 1,
            reason: Reason::NoSlot,
            waits_for: None,
        };
        let mut slots = Slots {
            limit: 2,
            in_use: 2,
            waiting: vec![waiting(Priority::Review, RunKind::Review)],
        };
        assert_eq!(reviews_line(&slots), "2 running, 1 waiting (limit 2)");
        slots
            .waiting
            .insert(0, waiting(Priority::Rebase, RunKind::Rebase));
        assert_eq!(
            reviews_line(&slots),
            "2 running, 2 waiting, 1 rebase first (limit 2)"
        );
    }

    #[test]
    fn each_sandbox_host_has_a_row_and_a_full_one_warns_while_runs_wait() {
        let host = |id: &str, limit, in_use| HostSlots {
            id: id.to_owned(),
            limit,
            in_use,
        };
        let now = SandboxHosts {
            hosts: vec![host("fox", None, 3), host("goat", Some(2), 2)],
            waiting: 1,
        };
        let row =
            |id: &str, state: &str, text: &str| (id.to_owned(), state.to_owned(), text.to_owned());
        assert_eq!(
            sandbox_host_rows(&now),
            [
                row(
                    "sandbox host fox",
                    "ok",
                    "3 in use, 1 waiting for a sandbox host"
                ),
                row(
                    "sandbox host goat",
                    "warn",
                    "2 of 2 in use, 1 waiting for a sandbox host"
                ),
            ]
        );
        let idle = SandboxHosts {
            hosts: vec![host("goat", Some(2), 2)],
            waiting: 0,
        };
        assert_eq!(
            sandbox_host_rows(&idle),
            [row("sandbox host goat", "ok", "2 of 2 in use")],
            "full, but nobody waits"
        );
        assert!(sandbox_host_rows(&SandboxHosts::default()).is_empty());
    }
}
