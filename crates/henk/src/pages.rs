//! The few server-made HTML pages left since the dashboard became an app
//! (#201): sign-in notices and "not served here". Every value is escaped.

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
