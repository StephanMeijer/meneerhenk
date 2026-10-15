//! GitLab: the writer over a write-mode MCP session, and the few REST reads
//! pushing and a merge request's CI (#420) need besides.

pub mod ci;
pub mod issues;
pub mod push;
pub mod rest;
pub mod writer;

pub use rest::GitLabRest;
pub use writer::GitLabWriter;

use serde_json::Value;

/// The web address of the GitLab an API URL belongs to: the URL without a
/// trailing slash and its `/api/v4`, scheme kept.
#[must_use]
pub fn web_base(api_url: &str) -> &str {
    let api = api_url.trim_end_matches('/');
    api.strip_suffix("/api/v4").unwrap_or(api)
}

/// A number or id GitLab or its MCP server may send as a JSON number or, as
/// GraphQL does, as a string.
pub(crate) fn number(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// The string at `key`, or empty when it is missing or not a string.
pub(crate) fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}
