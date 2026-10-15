//! GitLab: the writer over a write-mode MCP session, and the few REST reads
//! pushing and a merge request's CI (#420) need besides.

pub mod ci;
pub mod issues;
pub mod push;
pub mod rest;
pub mod writer;

pub use rest::GitLabRest;
pub use writer::GitLabWriter;

/// The web address of the GitLab an API URL belongs to: the URL without a
/// trailing slash and its `/api/v4`, scheme kept.
#[must_use]
pub fn web_base(api_url: &str) -> &str {
    let api = api_url.trim_end_matches('/');
    api.strip_suffix("/api/v4").unwrap_or(api)
}
