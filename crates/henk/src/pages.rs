//! HTML for runs and events, shown on the dashboard behind sign-in (#36,
//! #69); the run links posted in comments lead there. Every value that comes from
//! the store or a payload is escaped: it is other people's text (§8.3).

use std::fmt::Write as _;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use henk_domain::run::{EventId, RunId};
use henk_store::{EventRecord, FindingRecord, LaneRecord, RunStatus, RunStore};

use crate::runs::{PartView, TranscriptView};

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

/// Where links between runs and events point, and what a page may offer.
#[derive(Debug, Clone, Copy)]
pub struct Links<'a> {
    /// Prefix of run and event paths, such as `/dashboard`.
    pub prefix: &'static str,
    /// The menu above the page.
    pub nav: &'a str,
    /// The viewer's CSRF token, when the page may offer actions (#69).
    pub csrf: Option<&'a str>,
}

impl Links<'_> {
    /// The run page of `run`.
    #[must_use]
    pub fn run(self, run: &str) -> String {
        format!("{}/runs/{}", self.prefix, escape(run))
    }

    /// The page of `session`'s transcript on `run` (#191).
    #[must_use]
    pub fn transcript(self, run: &str, session: &str) -> String {
        format!(
            "{}/transcripts/{}",
            self.run(run),
            escape(&path_segment(session))
        )
    }

    /// The event page of `event`.
    #[must_use]
    pub fn event(self, event: &str) -> String {
        format!("{}/events/{}", self.prefix, escape(event))
    }
}

/// `text` as one URL path segment: everything but unreserved characters is
/// percent-escaped, so a session name can never leave its segment.
fn path_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A run with its lanes, findings, timeline and the events that led to it.
pub async fn run_page(store: &dyn RunStore, run_id: &RunId, links: Links<'_>) -> Response {
    let run = match store.run(run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let lanes = store.lanes(run_id).await.unwrap_or_default();
    let tools =
        henk_store::ToolUsage::from_calls(&store.tool_calls(run_id).await.unwrap_or_default());
    let transcripts: Vec<String> = store
        .transcripts(run_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.session)
        .collect();
    let drafts = store.drafts(run_id).await.unwrap_or_default();
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
    if let Some(csrf) = links.csrf
        && run.status == RunStatus::Running
    {
        let _ = write!(
            html,
            "<form method=\"post\" action=\"{}/cancel\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button>Cancel this run</button></form>",
            links.run(run.id.as_str()),
            escape(csrf)
        );
    }
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
    html.push_str(&lanes_table(&lanes, &transcripts, |session| {
        links.transcript(run.id.as_str(), session)
    }));
    html.push_str(&tools_table(&tools));
    html.push_str(&drafts_table(&drafts));
    html.push_str(&findings_table(&findings));
    html.push_str(&timeline_table(&events));
    page(&format!("Run {}", run.id), links.nav, &html)
}

/// What the lanes drafted and what became of each draft (#189): the
/// verdict, the model that gave it, what it repeats and the comment.
fn drafts_table(drafts: &[henk_store::DraftRecord]) -> String {
    let mut html = String::new();
    if drafts.is_empty() {
        return html;
    }
    html.push_str("<h2>Drafts</h2><table><tr><th>Draft</th><th>Lane</th><th>Where</th><th>What</th><th>Verdict</th><th>By</th><th>Comment</th><th>Reason</th></tr>");
    for draft in drafts {
        let what = match draft.kind.as_str() {
            "finding" => escape(&draft.body),
            kind => format!(
                "{} of {}: {}",
                escape(kind),
                escape(&draft.target),
                escape(&draft.body)
            ),
        };
        let (verdict, by, comment, reason) = match &draft.decision {
            Some(decision) => {
                let mut verdict = decision.verdict.as_str().replace('_', " ");
                if !decision.same_as.is_empty() {
                    verdict = format!("{verdict} {}", decision.same_as);
                }
                (
                    escape(&verdict),
                    escape(&decision.checker),
                    escape(&decision.comment_id),
                    escape(&decision.reason),
                )
            }
            None => (
                "waiting".to_owned(),
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td><code>{}:{}</code></td><td>{what}</td><td>{verdict}</td><td>{by}</td><td>{comment}</td><td>{reason}</td></tr>",
            escape(&draft.draft),
            escape(&draft.lane),
            escape(&draft.path),
            draft.line
        );
    }
    html.push_str("</table>");
    html
}

/// A tool result longer than this many lines is folded away.
const FOLD_LINES: usize = 12;

/// One session's whole conversation (#191): what the model was told, what
/// it answered and what each tool returned. All of it is escaped; most of
/// it is other people's text or the model's (§8.3).
pub async fn transcript_page(
    store: &dyn RunStore,
    run_id: &RunId,
    session: &str,
    links: Links<'_>,
) -> Response {
    let stored = match store.transcript(run_id, session).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such transcript").into_response(),
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let view = match TranscriptView::parse(&stored.body) {
        Ok(view) => view,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the stored transcript does not read: {error}"),
            )
                .into_response();
        }
    };
    let mut html = String::new();
    let _ = write!(
        html,
        "<h1>Transcript of {}</h1><p>Run <a href=\"{}\">{}</a><br>Model {}, stopped {}, {} turns, tokens in {} out {}<br>Recorded {}, {} bytes</p><h2>System prompt</h2><pre>{}</pre>",
        escape(&view.session),
        links.run(run_id.as_str()),
        escape(run_id.as_str()),
        escape(&view.model),
        escape(&view.stop),
        view.turns,
        view.tokens.0,
        view.tokens.1,
        escape(&stored.at),
        stored.bytes,
        escape(&view.system)
    );
    for message in &view.messages {
        let _ = write!(
            html,
            "<h2>{} <span class=\"muted\">turn {}</span></h2>",
            escape(&message.role),
            message.turn
        );
        for part in &message.parts {
            html.push_str(&part_html(part));
        }
    }
    page(
        &format!("Transcript of {} on {}", view.session, run_id),
        links.nav,
        &html,
    )
}

fn part_html(part: &PartView) -> String {
    match part {
        PartView::Text(text) => format!("<pre>{}</pre>", escape(text)),
        PartView::Call { name, arguments } => format!(
            "<p>Call <code>{}</code></p><pre>{}</pre>",
            escape(name),
            escape(arguments)
        ),
        PartView::Result { error, content } => {
            let label = if *error { "Result: error" } else { "Result" };
            let lines = content.lines().count();
            if lines > FOLD_LINES {
                format!(
                    "<details><summary>{label}, {lines} lines</summary><pre>{}</pre></details>",
                    escape(content)
                )
            } else {
                format!("<p>{label}</p><pre>{}</pre>", escape(content))
            }
        }
        PartView::Opaque => "<p class=\"muted\">Provider content, not shown.</p>".to_owned(),
    }
}

/// One inbound event with what each listener did with it.
pub async fn event_page(store: &dyn RunStore, event_id: &EventId, links: Links<'_>) -> Response {
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
        "<h1>Event {}</h1><p>Received {}<br>Source: <b>{}</b><br>Kind: <b>{}</b>{}{}</p>",
        escape(event.id.as_str()),
        escape(&event.received_at),
        escape(&event.source),
        escape(&event.kind),
        match (&event.repo, event.target) {
            (Some(repo), Some(target)) => format!("<br>About: {} #{target}", escape(repo)),
            (Some(repo), None) => format!("<br>About: {}", escape(repo)),
            _ => String::new(),
        },
        event
            .requester
            .as_deref()
            .map(|r| format!("<br>Asked by: {}", escape(r)))
            .unwrap_or_default(),
    );
    html.push_str("<h2>What the listeners did</h2>");
    if outcomes.is_empty() {
        html.push_str("<p>No listener has answered yet. Reload in a moment.</p>");
    }
    html.push_str(
        "<table><tr><th>Listener</th><th>Outcome</th><th>Detail</th><th>Run</th><th>At</th></tr>",
    );
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

/// The lanes, each linking to its transcript when one is stored.
fn lanes_table(
    lanes: &[LaneRecord],
    transcripts: &[String],
    link: impl Fn(&str) -> String,
) -> String {
    let mut html = String::new();
    if lanes.is_empty() {
        return html;
    }
    html.push_str("<h2>Lanes</h2><table><tr><th>Lane</th><th>Model</th><th>Status</th><th>Turns</th><th>Tokens in</th><th>Tokens out</th><th>Error</th></tr>");
    for lane in lanes {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{:?}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            if transcripts.contains(&lane.name) {
                format!(
                    "<a href=\"{}\" title=\"The whole conversation\">{}</a>",
                    link(&lane.name),
                    escape(&lane.name)
                )
            } else {
                escape(&lane.name)
            },
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

/// The tool calls of each session: calls, errors, refusals, the rest and
/// their time. Never their arguments.
fn tools_table(tools: &[henk_store::ToolUsage]) -> String {
    let mut html = String::new();
    if tools.is_empty() {
        return html;
    }
    html.push_str("<h2>Tool calls</h2><table><tr><th>Lane</th><th>Tool</th><th>Calls</th><th>Errors</th><th>Refused</th><th>Not run</th><th>Time (ms)</th></tr>");
    for usage in tools {
        let t = &usage.tally;
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&usage.session),
            escape(&usage.tool),
            t.calls,
            t.errors,
            t.refusals,
            t.other,
            t.total_ms
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_table_counts_per_lane_and_escapes_names() {
        let mut tally = henk_store::ToolTally::default();
        tally.add("ok", 3, 30);
        tally.add("refused_scope", 1, 0);
        let usage = henk_store::ToolUsage {
            session: "lane-<a>".to_owned(),
            model: String::new(),
            tool: "read_file".to_owned(),
            tally,
        };
        let html = tools_table(&[usage]);
        assert!(html.starts_with("<h2>Tool calls</h2>"), "{html}");
        assert!(
            html.contains("<tr><td>lane-&lt;a&gt;</td><td>read_file</td><td>4</td><td>0</td><td>1</td><td>0</td><td>30</td></tr>"),
            "{html}"
        );
        assert!(
            tools_table(&[]).is_empty(),
            "nothing for a run without calls"
        );
    }
}
