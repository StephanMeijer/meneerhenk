//! The few server-made HTML pages left since the dashboard became an app
//! (#201): sign-in notices and "not served here", as one card in the
//! dashboard's look (#232). Every value is escaped.

use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse, Response};

/// Escapes text for HTML content and attribute values.
#[must_use]
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The style every page shares: the dashboard's visual language (#224) in
/// light and dark, for the few pages the server makes itself (#232).
pub const STYLE: &str = concat!(
    ":root{color-scheme:light dark;--bg:#f4f5f7;--surface:#fff;--sunk:#edeff3;--ink:#13161b;",
    "--muted:#5b636f;--line:#c3c9d2;--accent:#1d5fd0;--on-accent:#fff}",
    "@media (prefers-color-scheme:dark){:root{--bg:#0c0f14;--surface:#13181f;--sunk:#192029;",
    "--ink:#e6eaf0;--muted:#8f99a7;--line:#333c48;--accent:#72a8ff;--on-accent:#0c0f14}}",
    "*{box-sizing:border-box}",
    "body{margin:0;min-height:100vh;display:grid;place-items:center;padding:1rem;",
    "background:var(--bg);color:var(--ink);font:15px/1.5 system-ui,-apple-system,'Segoe UI',sans-serif}",
    "main{width:min(30rem,100%);background:var(--surface);border:1px solid var(--line);",
    "border-radius:8px;padding:1.5rem}",
    ".brand{display:flex;align-items:center;gap:.6rem;font-weight:600;margin-bottom:1.25rem}",
    ".mark{display:inline-grid;place-items:center;width:28px;height:28px;border-radius:6px;",
    "background:var(--ink);color:var(--bg);font:700 13px ui-monospace,monospace}",
    "h1{font-size:22px;margin:0 0 .5rem}p{margin:0 0 1rem}",
    "code,.id{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px}",
    ".framed{background:var(--sunk);border:1px solid var(--line);border-radius:4px;padding:.6rem .8rem}",
    ".action{display:inline-block;background:var(--accent);color:var(--on-accent);",
    "text-decoration:none;font-weight:500;border-radius:4px;padding:.4rem .9rem}",
    ".hint{color:var(--muted);font-size:13px;margin:1rem 0 0}",
);

/// What a notice page says (#232).
#[derive(Debug, Clone, Copy, Default)]
pub struct Notice<'a> {
    /// The title: `Not for you`, `Sign-in failed`.
    pub title: &'a str,
    /// What happened, in a sentence or two.
    pub text: &'a str,
    /// A framed line for something to copy or pass on, such as an id.
    pub framed: Option<&'a str>,
    /// One way on: the button's words and where it goes.
    pub action: Option<(&'a str, &'a str)>,
    /// A last, quieter line.
    pub hint: Option<&'a str>,
}

/// A notice page: the card with the brand, the title, the text and what
/// comes with it. Every value is escaped.
#[must_use]
pub fn notice_page(notice: &Notice<'_>) -> Response {
    let framed = notice.framed.map_or_else(String::new, |framed| {
        format!("<p class=\"framed\">{}</p>", escape(framed))
    });
    let action = notice.action.map_or_else(String::new, |(label, href)| {
        format!(
            "<p><a class=\"action\" href=\"{}\">{}</a></p>",
            escape(href),
            escape(label)
        )
    });
    let hint = notice.hint.map_or_else(String::new, |hint| {
        format!("<p class=\"hint\">{}</p>", escape(hint))
    });
    let body = format!(
        "<h1>{}</h1><p>{}</p>{framed}{action}{hint}",
        escape(notice.title),
        escape(notice.text)
    );
    page(notice.title, "", &body)
}

/// A whole page. `nav` goes above the body; the dashboard puts its menu there.
#[must_use]
pub fn page(title: &str, nav: &str, body: &str) -> Response {
    let mut response = Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title><style>{STYLE}</style></head><body><main>{nav}<div class=\"brand\"><span class=\"mark\" aria-hidden=\"true\">H</span>Meneer Henk</div>{body}</main></body></html>",
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::body::to_bytes;

    use super::*;

    async fn html(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_notice_escapes_everything_and_says_it_in_style() {
        let page = html(notice_page(&Notice {
            title: "Not <b>for</b> you",
            text: "<script>alert(1)</script>",
            framed: Some("github:1 & \"2\""),
            action: Some(("Go", "/dashboard/login?next=\"x\"")),
            hint: Some("A hint."),
        }))
        .await;
        assert!(!page.contains("<script>alert"), "{page}");
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("<title>Not &lt;b&gt;for&lt;/b&gt; you</title>"));
        assert!(page.contains("github:1 &amp; &quot;2&quot;"));
        assert!(page.contains("href=\"/dashboard/login?next=&quot;x&quot;\""));
        assert!(page.contains("class=\"hint\">A hint.</p>"));
        assert!(page.contains("prefers-color-scheme:dark"), "light and dark");
        for line in [
            "This GitHub account may not see Henk's dashboard.",
            "Signed in at GitHub as github:88120. Ask an operator to add this id.",
            "To use another GitHub account, sign out of GitHub first.",
            "The sign-in expired or did not start here. Try again.",
        ] {
            assert!(
                henk_domain::text::style_violations(line).is_empty(),
                "{line}"
            );
        }
    }
}
