//! GitHub: App authentication, a small REST and GraphQL client, and the writer.

pub mod address;
pub mod api;
pub mod app;
pub mod issues;
pub mod writer;

pub use api::{GitHubApi, NewCheckRun};
pub use app::{AppCredentials, GitHubAuth};
pub use writer::GitHubWriter;

/// The web address of the GitHub an API base belongs to, without a trailing
/// slash: `https://github.com` for `https://api.github.com`, and a GitHub
/// Enterprise base without its `/api/v3`. Clone URLs, dashboard links and
/// the URLs Henk accepts all come from this, so they name the same host.
#[must_use]
pub fn web_base(api_base: &str) -> String {
    let api = api_base.trim_end_matches('/');
    if api == "https://api.github.com" {
        "https://github.com".to_owned()
    } else {
        api.strip_suffix("/api/v3").unwrap_or(api).to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::web_base;

    #[test]
    fn the_web_base_is_the_api_base_without_its_api_path() {
        assert_eq!(web_base("https://api.github.com"), "https://github.com");
        assert_eq!(web_base("https://api.github.com/"), "https://github.com");
        assert_eq!(
            web_base("https://ghe.example/api/v3"),
            "https://ghe.example"
        );
        assert_eq!(
            web_base("https://api.ghe.corp.com/api/v3/"),
            "https://api.ghe.corp.com",
            "an Enterprise host keeps its api. prefix"
        );
    }
}
