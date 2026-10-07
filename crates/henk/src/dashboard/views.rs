//! The dashboard's pages. Everything shown comes from the store, the
//! settings or payloads, and is escaped (§8.3).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use henk_store::{EventFilter, Page, RunFilter, RunRecord, RunStatus};
use serde_json::json;

use super::Dashboard;
use super::auth::{Viewer, encode};
use crate::config::Settings;
use crate::doctor::{Verdict, check_secrets};
use crate::pages::{Links, escape, event_page, page, run_page, transcript_page};

/// Rows per page.
const PER_PAGE: u32 = 50;

/// The menu above every page. Signing out is a form with the viewer's
/// token: a link would be a GET, and a form without a token is refused.
fn nav(csrf: &str) -> String {
    format!(
        "<nav><a href=\"/dashboard\">Runs</a><a href=\"/dashboard/events\">Events</a><a href=\"/dashboard/health\">Health</a><form method=\"post\" action=\"/dashboard/logout\" style=\"display:inline\">{}<button>Sign out</button></form></nav>",
        csrf_field(csrf)
    )
}

/// The hidden field that carries a form's CSRF token.
pub(super) fn csrf_field(csrf: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf\" value=\"{}\">",
        escape(csrf)
    )
}

/// Paths of run and event pages; for links only, not for a whole page.
const LINKS: Links<'static> = Links {
    prefix: "/dashboard",
    nav: "",
    csrf: None,
};

const KINDS: &[(&str, RunKind)] = &[
    ("review", RunKind::Review),
    ("plan", RunKind::Plan),
    ("discord_turn", RunKind::DiscordTurn),
    ("mail_reply", RunKind::MailReply),
    ("address", RunKind::Address),
];

const STATUSES: &[(&str, RunStatus)] = &[
    ("running", RunStatus::Running),
    ("finished", RunStatus::Finished),
    ("failed", RunStatus::Failed),
    ("cancelled", RunStatus::Cancelled),
];

const PLATFORMS: &[(&str, Platform)] =
    &[("github", Platform::GitHub), ("gitlab", Platform::GitLab)];

fn lookup<T: Copy>(table: &[(&str, T)], value: Option<&String>) -> Option<T> {
    let value = value?;
    table
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, v)| *v)
}

fn wanted(query: &HashMap<String, String>, key: &str) -> Option<String> {
    query
        .get(key)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn page_number(query: &HashMap<String, String>) -> u32 {
    query
        .get("page")
        .and_then(|p| p.parse::<u32>().ok())
        .unwrap_or(0)
        .min(10_000)
}

/// A link to the pull request, merge request or issue a run is about.
fn target_url(settings: &Settings, run: &RunRecord) -> Option<String> {
    let issue = run.kind == RunKind::Plan;
    match run.platform {
        Platform::GitHub => {
            let api = settings
                .github
                .as_ref()
                .map_or("https://api.github.com", |g| g.api_base.as_str());
            let web = if api.trim_end_matches('/') == "https://api.github.com" {
                "https://github.com".to_owned()
            } else {
                api.trim_end_matches('/')
                    .trim_end_matches("/api/v3")
                    .to_owned()
            };
            let what = if issue { "issues" } else { "pull" };
            Some(format!("{web}/{}/{what}/{}", run.repo, run.target))
        }
        Platform::GitLab => {
            let api = settings.gitlab.as_ref()?.api_url.trim_end_matches('/');
            let web = api.trim_end_matches("/api/v4");
            let what = if issue { "issues" } else { "merge_requests" };
            Some(format!("{web}/{}/-/{what}/{}", run.repo, run.target))
        }
    }
}

fn runs_table(settings: &Settings, runs: &[RunRecord], tbody_id: &str) -> String {
    let mut html = String::from(
        "<table><thead><tr><th>Run</th><th>Kind</th><th>About</th><th>Status</th><th>Started</th><th>Trigger</th></tr></thead>",
    );
    let _ = write!(html, "<tbody id=\"{tbody_id}\">");
    for run in runs {
        let about = format!("{} #{}", run.repo, run.target);
        let about = match target_url(settings, run) {
            Some(url) => format!("<a href=\"{}\">{}</a>", escape(&url), escape(&about)),
            None => escape(&about),
        };
        let _ = write!(
            html,
            "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td>{about}</td><td>{:?}</td><td>{}</td><td>{}</td></tr>",
            LINKS.run(run.id.as_str()),
            escape(run.id.as_str()),
            escape(&run.kind.to_string()),
            run.status,
            escape(&run.started_at),
            escape(&run.trigger)
        );
    }
    html.push_str("</tbody></table>");
    if runs.is_empty() {
        html.push_str("<p class=\"muted\">None.</p>");
    }
    html
}

fn select(name: &str, options: &[&str], chosen: Option<&String>) -> String {
    let mut html = format!("<label>{name} <select name=\"{name}\"><option value=\"\">any</option>");
    for option in options {
        let selected = if chosen.is_some_and(|c| c == option) {
            " selected"
        } else {
            ""
        };
        let _ = write!(html, "<option{selected}>{option}</option>");
    }
    html.push_str("</select></label>");
    html
}

/// Previous and next links that keep the filters.
fn pager(base: &str, query: &HashMap<String, String>, page: u32, full: bool) -> String {
    let keep: String = {
        let mut pairs: Vec<(&String, &String)> = query
            .iter()
            .filter(|(k, v)| *k != "page" && !v.is_empty())
            .collect();
        pairs.sort();
        pairs.iter().fold(String::new(), |mut acc, (k, v)| {
            let _ = write!(acc, "{}={}&", encode(k), encode(v));
            acc
        })
    };
    let mut html = String::from("<p>");
    if page > 0 {
        let _ = write!(
            html,
            "<a href=\"{base}?{keep}page={}\">Newer</a> ",
            page - 1
        );
    }
    if full {
        let _ = write!(html, "<a href=\"{base}?{keep}page={}\">Older</a>", page + 1);
    }
    html.push_str("</p>");
    html
}

/// The form that starts work, as `POST /review`, `/plan` and `/address`
/// would (#69). An address run is offered only when configured.
fn start_form(settings: &Settings, csrf: &str) -> String {
    let address = if settings.address.is_some() {
        "<option value=\"address\">address the review feedback</option>"
    } else {
        ""
    };
    format!(
        "<h2>Start</h2><form class=\"filters\" method=\"post\" action=\"/dashboard/start\">{}\
         <label>what <select name=\"kind\"><option value=\"review\">review a pull request</option>\
         <option value=\"plan\">plan an issue</option>{address}</select></label>\
         <label>URL <input name=\"url\" required placeholder=\"https://github.com/owner/name/pull/7\"></label>\
         <label>commit <input name=\"commit\" placeholder=\"review only; default the head\"></label>\
         <label>note <input name=\"note\" placeholder=\"plan or address only\"></label>\
         <button>Start</button></form>",
        csrf_field(csrf)
    )
}

/// Runs: what is running now, and every run by filter.
pub async fn overview(
    State(dashboard): State<Arc<Dashboard>>,
    Viewer(viewer): Viewer,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let store = &dashboard.app.store;
    let settings = &dashboard.app.settings;
    let (running, running_total) = running_now(&dashboard).await;
    let filter = RunFilter {
        kind: lookup(KINDS, query.get("kind")),
        status: lookup(STATUSES, query.get("status")),
        platform: lookup(PLATFORMS, query.get("platform")),
        repo: wanted(&query, "repo"),
    };
    let number = page_number(&query);
    let runs = match store
        .list_runs(&filter, Page::new(PER_PAGE, number * PER_PAGE))
        .await
    {
        Ok(runs) => runs,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };

    let token = dashboard.signer.csrf(&viewer);
    let mut html = format!(
        "<h1>Meneer Henk</h1><p class=\"muted\">Signed in as {}.</p>",
        escape(&viewer.login)
    );
    html.push_str(&start_form(settings, &token));
    let _ = write!(
        html,
        "<h2>Running now (<span id=\"running-count\">{running_total}</span>)</h2>{}",
        runs_table(settings, &running, "running")
    );
    let more = more_running(running_total, running.len());
    let _ = write!(
        html,
        "<p class=\"muted\" id=\"running-more\"{}>{more}</p>",
        if more.is_empty() { " hidden" } else { "" }
    );
    html.push_str("<h2>Runs</h2><form class=\"filters\" method=\"get\">");
    html.push_str(&select(
        "kind",
        &KINDS.iter().map(|k| k.0).collect::<Vec<_>>(),
        query.get("kind"),
    ));
    html.push_str(&select(
        "status",
        &STATUSES.iter().map(|k| k.0).collect::<Vec<_>>(),
        query.get("status"),
    ));
    html.push_str(&select(
        "platform",
        &PLATFORMS.iter().map(|k| k.0).collect::<Vec<_>>(),
        query.get("platform"),
    ));
    let _ = write!(
        html,
        "<label>repo <input name=\"repo\" value=\"{}\" placeholder=\"owner/name\"></label><button>Show</button></form>",
        escape(query.get("repo").map_or("", String::as_str))
    );
    html.push_str(&runs_table(settings, &runs, "runs"));
    html.push_str(&pager(
        "/dashboard",
        &query,
        number,
        u32::try_from(runs.len()).unwrap_or(0) == PER_PAGE,
    ));
    html.push_str("<script src=\"/dashboard/app.js\"></script>");
    page("Meneer Henk", &nav(&token), &html)
}

/// One run.
pub async fn run(
    State(dashboard): State<Arc<Dashboard>>,
    Viewer(viewer): Viewer,
    Path(id): Path<String>,
) -> Response {
    let Ok(run_id) = RunId::parse(id) else {
        return (StatusCode::BAD_REQUEST, "bad run id").into_response();
    };
    let token = dashboard.signer.csrf(&viewer);
    let nav = nav(&token);
    let links = Links {
        nav: &nav,
        csrf: Some(&token),
        ..LINKS
    };
    run_page(dashboard.app.store.as_ref(), &run_id, links).await
}

/// One session's transcript on a run (#191).
pub async fn transcript(
    State(dashboard): State<Arc<Dashboard>>,
    Viewer(viewer): Viewer,
    Path((id, session)): Path<(String, String)>,
) -> Response {
    let Ok(run_id) = RunId::parse(id) else {
        return (StatusCode::BAD_REQUEST, "bad run id").into_response();
    };
    let token = dashboard.signer.csrf(&viewer);
    let nav = nav(&token);
    let links = Links {
        nav: &nav,
        csrf: Some(&token),
        ..LINKS
    };
    transcript_page(dashboard.app.store.as_ref(), &run_id, &session, links).await
}

/// Inbound events, with what each listener did.
pub async fn events(
    State(dashboard): State<Arc<Dashboard>>,
    Viewer(viewer): Viewer,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let filter = EventFilter {
        source: wanted(&query, "source"),
        kind: wanted(&query, "kind"),
        repo: wanted(&query, "repo"),
    };
    let number = page_number(&query);
    let listed = match dashboard
        .app
        .store
        .list_inbound_events(&filter, Page::new(PER_PAGE, number * PER_PAGE))
        .await
    {
        Ok(listed) => listed,
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let mut html = String::from("<h1>Events</h1><form class=\"filters\" method=\"get\">");
    for (name, hint) in [
        ("source", "github_webhook"),
        ("kind", "pull_request"),
        ("repo", "owner/name"),
    ] {
        let _ = write!(
            html,
            "<label>{name} <input name=\"{name}\" value=\"{}\" placeholder=\"{hint}\"></label>",
            escape(query.get(name).map_or("", String::as_str))
        );
    }
    html.push_str("<button>Show</button></form>");
    html.push_str("<table><thead><tr><th>Event</th><th>Received</th><th>Source</th><th>Kind</th><th>About</th><th>What the listeners did</th></tr></thead><tbody>");
    for item in &listed {
        let event = &item.event;
        let about = match (&event.repo, event.target) {
            (Some(repo), Some(target)) => format!("{} #{target}", escape(repo)),
            (Some(repo), None) => escape(repo),
            _ => String::new(),
        };
        let outcomes = item.outcomes.iter().fold(String::new(), |mut acc, o| {
            let run = o
                .run_id
                .as_deref()
                .map(|r| format!(" <a href=\"{}\">{}</a>", LINKS.run(r), escape(r)))
                .unwrap_or_default();
            let _ = write!(
                acc,
                "<div><b>{}</b>: {} <span class=\"muted\">{}</span>{run}</div>",
                escape(&o.listener),
                escape(&o.outcome),
                escape(&o.detail)
            );
            acc
        });
        let _ = write!(
            html,
            "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{about}</td><td>{outcomes}</td></tr>",
            LINKS.event(event.id.as_str()),
            escape(event.id.as_str()),
            escape(&event.received_at),
            escape(&event.source),
            escape(&event.kind)
        );
    }
    html.push_str("</tbody></table>");
    if listed.is_empty() {
        html.push_str("<p class=\"muted\">None.</p>");
    }
    html.push_str(&pager(
        "/dashboard/events",
        &query,
        number,
        u32::try_from(listed.len()).unwrap_or(0) == PER_PAGE,
    ));
    page("Events", &nav(&dashboard.signer.csrf(&viewer)), &html)
}

/// One event.
pub async fn event(
    State(dashboard): State<Arc<Dashboard>>,
    Viewer(viewer): Viewer,
    Path(id): Path<String>,
) -> Response {
    let Ok(event_id) = EventId::parse(id) else {
        return (StatusCode::BAD_REQUEST, "bad event id").into_response();
    };
    let token = dashboard.signer.csrf(&viewer);
    let nav = nav(&token);
    let links = Links {
        nav: &nav,
        csrf: Some(&token),
        ..LINKS
    };
    event_page(dashboard.app.store.as_ref(), &event_id, links).await
}

/// What the service has, from configuration and the store. Nothing here
/// starts a process or calls a model; that is `henk doctor --probe`.
pub async fn health(State(dashboard): State<Arc<Dashboard>>, Viewer(viewer): Viewer) -> Response {
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
        format!("{} running", dashboard.coordinator.active_reviews()),
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
    let mut html = String::from(
        "<h1>Health</h1><p class=\"muted\">From configuration and the database. <code>henk doctor --probe</code> also tries every model and MCP server.</p><table><tr><th>Check</th><th>State</th><th>Detail</th></tr>",
    );
    for (name, verdict, text) in &rows {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(name),
            escape(verdict),
            escape(text)
        );
    }
    html.push_str("</table>");
    page("Health", &nav(&dashboard.signer.csrf(&viewer)), &html)
}

/// The newest page of running runs, and how many are running in all: the
/// list stops at [`Page::MAX`], the count does not.
async fn running_now(dashboard: &Dashboard) -> (Vec<RunRecord>, u64) {
    let filter = RunFilter {
        status: Some(RunStatus::Running),
        ..RunFilter::default()
    };
    let store = &dashboard.app.store;
    let runs = store
        .list_runs(&filter, Page::new(Page::MAX, 0))
        .await
        .unwrap_or_default();
    let shown = u64::try_from(runs.len()).unwrap_or(u64::MAX);
    // A run can finish between the two queries; never report fewer than shown.
    let total = store
        .count_runs(&filter)
        .await
        .map_or(shown, |total| total.max(shown));
    (runs, total)
}

/// The line under a running list that does not show every running run.
fn more_running(total: u64, shown: usize) -> String {
    let shown = u64::try_from(shown).unwrap_or(u64::MAX);
    match total.saturating_sub(shown) {
        0 => String::new(),
        more => format!("And {more} more not shown."),
    }
}

/// The running runs, for the overview's poller: `total` is how many are
/// running, `runs` the newest of them.
pub async fn running_json(State(dashboard): State<Arc<Dashboard>>, _viewer: Viewer) -> Response {
    let (runs, total) = running_now(&dashboard).await;
    let settings = &dashboard.app.settings;
    let rows: Vec<_> = runs
        .iter()
        .map(|run| {
            json!({
                "id": run.id.as_str(),
                "kind": run.kind.to_string(),
                "about": format!("{} #{}", run.repo, run.target),
                "about_url": target_url(settings, run),
                "status": format!("{:?}", run.status),
                "started_at": run.started_at,
                "trigger": run.trigger,
            })
        })
        .collect();
    Json(json!({ "total": total, "runs": rows })).into_response()
}

/// Refreshes "Running now" every five seconds. Builds the rows with
/// `textContent`, never as HTML, so no stored text can become markup.
const APP_JS: &str = r#""use strict";
(function () {
  const body = document.getElementById("running");
  const count = document.getElementById("running-count");
  const more = document.getElementById("running-more");
  if (!body || !count || !more) return;
  function cell(text, href) {
    const td = document.createElement("td");
    if (href) {
      const a = document.createElement("a");
      a.href = href;
      a.textContent = text;
      td.append(a);
    } else {
      td.textContent = text;
    }
    return td;
  }
  function row(run) {
    const tr = document.createElement("tr");
    tr.append(
      cell(run.id, "/dashboard/runs/" + encodeURIComponent(run.id)),
      cell(run.kind),
      cell(run.about, run.about_url),
      cell(run.status),
      cell(run.started_at),
      cell(run.trigger)
    );
    return tr;
  }
  async function refresh() {
    try {
      const response = await fetch("/dashboard/running.json", { credentials: "same-origin" });
      if (!response.ok) return;
      const running = await response.json();
      body.replaceChildren(...running.runs.map(row));
      count.textContent = String(running.total);
      const hidden = running.total - running.runs.length;
      more.textContent = hidden > 0 ? "And " + hidden + " more not shown." : "";
      more.hidden = hidden <= 0;
    } catch (_) {
      // The next tick tries again.
    }
  }
  setInterval(refresh, 5000);
})();
"#;

/// The poller script.
pub async fn app_js(_viewer: Viewer) -> Response {
    let mut response = APP_JS.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/javascript; charset=utf-8"),
    );
    response
}
