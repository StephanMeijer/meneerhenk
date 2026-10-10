//! The few GitLab REST reads an address run (§3.5) needs that the MCP
//! server does not give: the merge request's source and target project, a
//! branch's protection, and who the token belongs to. Read-only; every
//! write still goes through the write-mode MCP session.
//!
//! The token travels only in the `PRIVATE-TOKEN` header, marked sensitive,
//! never in a URL. Redirects are not followed, so it never reaches another
//! host.

use std::fmt::Write as _;

use reqwest::header::HeaderValue;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value;
use tracing::instrument;

use crate::address::GitCredential;
use crate::error::{PlatformError, truncate};

const USER_AGENT: &str = "meneer-henk (https://github.com/StephanMeijer/meneerhenk)";

/// A read-only GitLab REST client bound to one personal access token.
pub struct GitLabRest {
    http: reqwest::Client,
    api_url: String,
    token: SecretString,
}

impl std::fmt::Debug for GitLabRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabRest")
            .field("api_url", &self.api_url)
            .finish_non_exhaustive()
    }
}

/// One path segment, percent-encoded: GitLab takes a project path such as
/// `group/sub/name` and a branch such as `feat/x` as a single segment.
#[must_use]
pub fn path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

impl GitLabRest {
    /// Builds a client for `api_url`, such as `https://gitlab.com/api/v4`.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] when the HTTP client cannot be built.
    pub fn new(api_url: &str, token: SecretString) -> Result<Self, PlatformError> {
        henk_llm::ensure_tls_provider();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_mins(1))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| PlatformError::Auth(format!("http client: {e}")))?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_owned(),
            token,
        })
    }

    /// The web address of the instance: the API URL without `/api/v4`.
    #[must_use]
    pub fn web_url(&self) -> &str {
        super::web_base(&self.api_url)
    }

    /// The instance's host name, for the noreply commit address.
    #[must_use]
    pub fn host(&self) -> String {
        reqwest::Url::parse(&self.api_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "gitlab.com".to_owned())
    }

    /// The credential git pushes with: the same token, as `oauth2`.
    #[must_use]
    pub fn git_credential(&self) -> GitCredential {
        GitCredential::gitlab(SecretString::from(self.token.expose_secret().to_owned()))
    }

    /// `GET` a path under the API; `None` when GitLab answers 404.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on a transport failure, another error
    /// status, or a body that is not JSON.
    #[instrument(skip(self))]
    pub async fn get_optional(&self, path: &str) -> Result<Option<Value>, PlatformError> {
        let mut token = HeaderValue::from_str(self.token.expose_secret()).map_err(|_| {
            PlatformError::Auth("the GitLab token is not a valid header".to_owned())
        })?;
        token.set_sensitive(true);
        let response = self
            .http
            .get(format!("{}{path}", self.api_url))
            .header("PRIVATE-TOKEN", token)
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response.text().await?;
        if !status.is_success() {
            return Err(PlatformError::Status {
                status: status.as_u16(),
                body: truncate(&body),
            });
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| PlatformError::Decode(format!("{e}: {}", truncate(&body))))
    }

    /// `GET` a path under the API.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Status`] with 404 when it is not there, or
    /// another [`PlatformError`] as [`Self::get_optional`] does.
    pub async fn get(&self, path: &str) -> Result<Value, PlatformError> {
        self.get_optional(path)
            .await?
            .ok_or_else(|| PlatformError::Status {
                status: 404,
                body: format!("{path} not found"),
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_project_path_or_branch_is_one_segment() {
        assert_eq!(path_segment("9xxlab/tools/cli"), "9xxlab%2Ftools%2Fcli");
        assert_eq!(path_segment("feat/a b#1"), "feat%2Fa%20b%231");
        assert_eq!(path_segment("v1.2_x-y~z"), "v1.2_x-y~z");
    }

    #[test]
    fn the_web_url_and_host_come_from_the_api_url() {
        let token = || SecretString::from("glpat-x".to_owned());
        let rest = GitLabRest::new("https://gitlab.example/api/v4/", token()).unwrap();
        assert_eq!(rest.web_url(), "https://gitlab.example");
        assert_eq!(rest.host(), "gitlab.example");
        let nested = GitLabRest::new("https://h.test/gitlab/api/v4", token()).unwrap();
        assert_eq!(nested.web_url(), "https://h.test/gitlab");
        assert!(!format!("{rest:?}").contains("glpat-x"));
    }
}
