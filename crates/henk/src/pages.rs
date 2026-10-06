//! HTML for runs and events, shown on the dashboard behind sign-in (#36,
//! #69); the run links posted in comments lead there. Every value that comes from
//! the store or a payload is escaped: it is other people's text (§8.3).

use std::fmt::Write as _;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use henk_domain::run::{EventId, RunId};
use henk_store::{EventRecord, FindingRecord, LaneRecord, RunStore};

/// Escapes text for HTML content and attribute values.
#[must_use]
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The style every page shares.
pub const STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;max-width:72rem;margin:2rem auto;padding:0 1rem;color:#222}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:.3rem .6rem;border-bottom:1px solid #ddd;vertical-align:top}code{background:#f3f3f3;padding:0 .2rem}pre{background:#f3f3f3;padding:.6rem;overflow:auto;max-height:30rem}nav a{margin-right:1rem}form.filters{margin:1rem 0}form.filters label{margin-right:.8rem}.muted{color:#777}";

/// A whole page. `nav` goes above the body; the dashboard puts its menu there.
#[must_use]
pub fn page(title: &str, nav: &str, body: &str) -> Response {
    let mut response = Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title><style>{STYLE}</style></head><body>{nav}{body}</body></html>",
        escape(title)
    ))
    .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Where links between runs and events point.
#[derive(Debug, Clone, Copy)]
pub struct Links {
    /// Prefix of run and event paths, such as `/dashboard`.
    pub prefix: &'static str,
    /// The menu above the page.
    pub nav: &'static str,
}

impl Links {
    /// The run page of `run`.
    #[must_use]
    pub fn run(self, run: &str) -> String {
        format!("{}/runs/{}", self.prefix, escape(run))
    }

    /// The event page of `event`.
    #[must_use]
    pub fn event(self, event: &str) -> String {
        format!("{}/events/{}", self.prefix, escape(event))
    }
}

/// A run with its lanes, findings, timeline and the events that led to it.
pub async fn run_page(store: &dyn RunStore, run_id: &RunId, links: Links) -> Response {
    let run = match store.run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let lanes = store.lanes(run_id).await.unwrap_or_default();
    let findings = store.findings(run_id).await.unwrap_or_default();
    let events = store.events(run_id).await.unwrap_or_default();
    let inbound = store
        .inbound_events_for_run(run_id)
        .await
        .unwrap_or_default();

    let mut html = String::new();
    let _ = write!(
        html,
        "<h1>Meneer Henk: {} {}</h1><p><b>{}</b> {} #{} {}<br>Status: <b>{:?}</b><br>Started {}{}<br>Trigger: {}{}{}</p>",
        escape(&run.kind.to_string()),
        escape(run.id.as_str()),
        escape(&format!("{:?}", run.platform)),
        escape(&run.repo),
        run.target,
        run.commit
            .as_deref()
            .map(|c| format!("at <code>{}</code>", escape(c)))
            .unwrap_or_default(),
        run.status,
        escape(&run.started_at),
        run.finished_at
            .as_deref()
            .map(|f| format!(", finished {}", escape(f)))
            .unwrap_or_default(),
        escape(&run.trigger),
        run.requester
            .as_deref()
            .map(|r| format!(" (asked by {})", escape(r)))
            .unwrap_or_default(),
        run.check_id
            .as_deref()
            .map(|c| format!("<br>Check: <code>{}</code>", escape(c)))
            .unwrap_or_default(),
    );
    if let Some(summary) = &run.summary {
        let _ = write!(html, "<p><b>Summary:</b> {}</p>", escape(summary));
    }
    if let Some(error) = &run.error {
        let _ = write!(html, "<p><b>Error:</b> <code>{}</code></p>", escape(error));
    }
    if !inbound.is_empty() {
        html.push_str("<h2>Events</h2><table><tr><th>Event</th><th>Received</th><th>Source</th><th>Kind</th></tr>");
        for event in &inbound {
            let _ = write!(
                html,
                "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                links.event(event.id.as_str()),
                escape(event.id.as_str()),
                escape(&event.received_at),
                escape(&event.source),
                escape(&event.kind)
            );
        }
        html.push_str("</table>");
    }
    html.push_str(&lanes_table(&lanes));
    html.push_str(&findings_table(&findings));
    html.push_str(&timeline_table(&events));
    page(&format!("Run {}", run.id), links.nav, &html)
}

/// One inbound event with what each listener did with it.
pub async fn event_page(store: &dyn RunStore, event_id: &EventId, links: Links) -> Response {
    let event = match store.inbound_event(event_id).await {
        Ok(Some(event)) => event,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such event").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let outcomes = store.outcomes(event_id).await.unwrap_or_default();
    let mut html = String::new();
    let _ = write!(
        html,
        "<h1>Event {}</h1><p>Received {}<br>Source: <b>{}</b><br>Kind: <b>{}</b>{}</p>",
        escape(event.id.as_str()),
        escape(&event.received_at),
        escape(&event.source),
        escape(&event.kind),
        match (&event.repo, event.target) {
            (Some(repo), Some(target)) => format!("<br>About: {} #{target}", escape(repo)),
            (Some(repo), None) => format!("<br>About: {}", escape(repo)),
            _ => String::new(),
        }
    );
    html.push_str("<h2>What the listeners did</h2><table><tr><th>Listener</th><th>Outcome</th><th>Detail</th><th>Run</th><th>At</th></tr>");
    for outcome in &outcomes {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&outcome.listener),
            escape(&outcome.outcome),
            escape(&outcome.detail),
            outcome
                .run_id
                .as_deref()
                .map(|r| format!("<a href=\"{}\">{}</a>", links.run(r), escape(r)))
                .unwrap_or_default(),
            escape(&outcome.at)
        );
    }
    html.push_str("</table>");
    if let Some(payload) = &event.payload {
        let _ = write!(
            html,
            "<h2>Payload as received</h2><pre>{}</pre>",
            escape(payload)
        );
    }
    page(&format!("Event {}", event.id), links.nav, &html)
}

fn lanes_table(lanes: &[LaneRecord]) -> String {
    let mut html = String::new();
    if lanes.is_empty() {
        return html;
    }
    html.push_str("<h2>Lanes</h2><table><tr><th>Lane</th><th>Model</th><th>Status</th><th>Turns</th><th>Tokens in</th><th>Tokens out</th><th>Error</th></tr>");
    for lane in lanes {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{:?}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&lane.name),
            escape(&lane.model),
            lane.status,
            lane.turns,
            lane.input_tokens,
            lane.output_tokens,
            escape(lane.error.as_deref().unwrap_or(""))
        );
    }
    html.push_str("</table>");
    html
}

fn findings_table(findings: &[FindingRecord]) -> String {
    let mut html = String::new();
    if findings.is_empty() {
        return html;
    }
    html.push_str("<h2>Findings</h2><table><tr><th>At</th><th>Lane</th><th>Where</th><th>Comment</th><th>What happened</th></tr>");
    for finding in findings {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td><code>{}:{}</code></td><td>{}</td><td>{}</td></tr>",
            escape(&finding.at),
            escape(&finding.lane),
            escape(&finding.path),
            finding.line,
            escape(&finding.comment_id),
            escape(&finding.action)
        );
    }
    html.push_str("</table>");
    html
}

fn timeline_table(events: &[EventRecord]) -> String {
    let mut html = String::new();
    if events.is_empty() {
        return html;
    }
    html.push_str("<h2>Timeline</h2><table>");
    for event in events {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&event.at),
            escape(&event.level),
            escape(&event.message)
        );
    }
    html.push_str("</table>");
    html
}
