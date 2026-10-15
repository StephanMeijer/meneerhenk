//! The few GitLab REST reads pushing to a merge request needs that the MCP
//! server does not give: the merge request's source and target project, a
//! branch's protection, and who the token belongs to; and its pipelines,
//! jobs and job logs (#420). Read-only; every
//! write still goes through the write-mode MCP session.
//!
//! The token travels only in the `PRIVATE-TOKEN` header, marked sensitive,
//! never in a URL. Redirects are not followed, so it never reaches another
//! host.

use reqwest::header::HeaderValue;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value;
use tracing::instrument;

use crate::error::{PlatformError, truncate};
use crate::push::GitCredential;

const USER_AGENT: &str = "meneer-henk (https://github.com/StephanMeijer/meneerhenk)";

/// Entries per page of a paged read.
const PAGE_SIZE: usize = 100;

/// Pages read before a list counts as too long, as on GitHub (#113).
const MAX_PAGES: usize = 100;

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
pub use crate::encode::percent_encode as path_segment;

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

    /// Sends a `GET` for a path under the API with the token.
    async fn send(&self, path: &str, accept: &str) -> Result<reqwest::Response, PlatformError> {
        let mut token = HeaderValue::from_str(self.token.expose_secret()).map_err(|_| {
            PlatformError::Auth("the GitLab token is not a valid header".to_owned())
        })?;
        token.set_sensitive(true);
        Ok(self
            .http
            .get(format!("{}{path}", self.api_url))
            .header("PRIVATE-TOKEN", token)
            .header("Accept", accept)
            .send()
            .await?)
    }

    /// `GET` a path under the API; `None` when GitLab answers 404.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on a transport failure, another error
    /// status, or a body that is not JSON.
    #[instrument(skip(self))]
    pub async fn get_optional(&self, path: &str) -> Result<Option<Value>, PlatformError> {
        let response = self.send(path, "application/json").await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let body = response.text().await?;
        if !status.is_success() {
            return Err(PlatformError::Status {
                status: status.as_u16(),
                body: truncate(&body),
                retry_after: None,
            });
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| PlatformError::Decode(format!("{e}: {}", truncate(&body))))
    }

    /// `GET` every page of a list under the API, to its end: a page shorter
    /// than `PAGE_SIZE` (100) is the last.
    ///
    /// # Errors
    ///
    /// As [`Self::get`], and [`PlatformError::TooMany`] when the list goes
    /// on past `MAX_PAGES` (100) full pages.
    pub async fn get_all(&self, path: &str) -> Result<Vec<Value>, PlatformError> {
        let mut items = Vec::new();
        let separator = if path.contains('?') { '&' } else { '?' };
        for page in 1..=MAX_PAGES + 1 {
            let page_items = self
                .get(&format!(
                    "{path}{separator}per_page={PAGE_SIZE}&page={page}"
                ))
                .await?;
            let Some(array) = page_items.as_array() else {
                return Ok(items);
            };
            if page > MAX_PAGES && !array.is_empty() {
                break;
            }
            let count = array.len();
            items.extend(array.iter().cloned());
            if count < PAGE_SIZE {
                return Ok(items);
            }
        }
        Err(PlatformError::TooMany {
            what: path.to_owned(),
            limit: MAX_PAGES * PAGE_SIZE,
        })
    }

    /// `GET` a path under the API as text, keeping only its last
    /// `max_bytes`, cut at a line: for a job's log.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Status`] on an error status, including a
    /// redirect, which is not followed, or a transport failure.
    #[instrument(skip(self))]
    pub async fn get_text_tail(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<crate::ci::CiLog, PlatformError> {
        let mut response = self.send(path, "text/plain").await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await?;
            return Err(PlatformError::Status {
                status: status.as_u16(),
                body: truncate(&body),
                retry_after: None,
            });
        }
        let mut tail = crate::ci::Tail::new(max_bytes);
        while let Some(chunk) = response.chunk().await? {
            tail.push(&chunk);
        }
        Ok(tail.finish())
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
                retry_after: None,
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
