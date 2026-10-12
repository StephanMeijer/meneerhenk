//! The GitHub REST and GraphQL calls Henk needs that the MCP server does not
//! provide safely: review comments at a commit, check runs, reactions,
//! comment edits, review-thread state, and minimising comments.

use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use tracing::instrument;

use henk_domain::allowlist::RepoRef;

use crate::error::{PlatformError, truncate};
use crate::github::app::{GitHubAuth, Installations, Scope};

/// A thin client bound to one authentication. With App authentication it
/// acts for one installation: build it once, then take
/// [`Self::for_repository`] for each repository's work.
#[derive(Debug)]
pub struct GitHubApi {
    http: reqwest::Client,
    api_base: String,
    graphql_url: String,
    auth: GitHubAuth,
}

const USER_AGENT: &str = "meneer-henk (https://github.com/StephanMeijer/meneerhenk)";

/// Entries per page of every paged read.
const PAGE_SIZE: usize = 100;

/// Pages read before a list counts as too long: 10,000 entries. Lists are
/// read to their end (#113); this cap only stops a runaway, and reaching it
/// is [`PlatformError::TooMany`], never a quietly partial list.
const MAX_PAGES: usize = 100;

/// A check run to create.
#[derive(Debug, Clone, Copy)]
pub struct NewCheckRun<'a> {
    /// The check's name.
    pub name: &'a str,
    /// The commit it is on.
    pub head_sha: &'a str,
    /// `queued` or `in_progress`.
    pub status: &'a str,
    /// Where it links to.
    pub details_url: &'a str,
    /// Henk's own id for it: the run link.
    pub external_id: &'a str,
    /// Its title and summary, when it says something from the start.
    pub output: Option<(&'a str, &'a str)>,
}

impl GitHubApi {
    /// Builds a client for `api_base` (normally `https://api.github.com`).
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] when the HTTP client cannot be built.
    pub fn new(api_base: &str, auth: GitHubAuth) -> Result<Self, PlatformError> {
        henk_llm::ensure_tls_provider();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_mins(1))
            .build()
            .map_err(|e| PlatformError::Auth(format!("http client: {e}")))?;
        let api_base = api_base.trim_end_matches('/').to_owned();
        let graphql_url = format!("{api_base}/graphql");
        Ok(Self {
            http,
            api_base,
            graphql_url,
            auth,
        })
    }

    fn scoped(&self, scope: Scope) -> Self {
        Self {
            http: self.http.clone(),
            api_base: self.api_base.clone(),
            graphql_url: self.graphql_url.clone(),
            auth: self.auth.scoped(scope),
        }
    }

    /// The same client acting for the installation that reaches `repo`,
    /// found on its first call. Every call it makes, and the git token it
    /// gives, is that installation's.
    #[must_use]
    pub fn for_repository(&self, repo: &RepoRef) -> Self {
        self.scoped(Scope::Repository {
            owner: repo.owner().to_owned(),
            name: repo.name().to_owned(),
        })
    }

    /// The same client acting for installation `id`.
    #[must_use]
    pub fn for_installation(&self, id: u64) -> Self {
        self.scoped(Scope::Installation(id))
    }

    /// The App's installations, when this client authenticates as an App.
    #[must_use]
    pub fn installations(&self) -> Option<&std::sync::Arc<Installations>> {
        self.auth.installations()
    }

    /// The installation this client acts for.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when it is not an App client scoped to a
    /// repository or installation, or the installation cannot be found.
    pub async fn installation(&self) -> Result<u64, PlatformError> {
        self.auth.installation(&self.http, &self.api_base).await
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, PlatformError> {
        self.request_accepting(method, url, "application/vnd.github+json")
            .await
    }

    async fn request_accepting(
        &self,
        method: reqwest::Method,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::RequestBuilder, PlatformError> {
        let token = self.auth.bearer(&self.http, &self.api_base).await?;
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(token.expose_secret())
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28"))
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<Value, PlatformError> {
        let response = builder.send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(PlatformError::Status {
                status: status.as_u16(),
                body: truncate(&body),
            });
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body)
            .map_err(|e| PlatformError::Decode(format!("{e}: {}", truncate(&body))))
    }

    /// The token git uses to clone and push as the App: the same cached
    /// installation token the API calls use. Never logged, never in argv.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when no token can be minted.
    pub async fn git_token(&self) -> Result<secrecy::SecretString, PlatformError> {
        self.auth.bearer(&self.http, &self.api_base).await
    }

    /// The HTTPS clone URL of `owner/name` on this GitHub: `github.com` for
    /// the public API, the host of a GitHub Enterprise `/api/v3` base.
    #[must_use]
    pub fn git_remote(&self, repo: &str) -> String {
        format!("{}/{repo}.git", super::web_base(&self.api_base))
    }

    /// `GET` a REST path.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport, status or decode failure.
    pub async fn get(&self, path: &str) -> Result<Value, PlatformError> {
        let builder = self
            .request(reqwest::Method::GET, &format!("{}{path}", self.api_base))
            .await?;
        self.send(builder).await
    }

    /// `GET` a REST path with a specific `Accept` and the body as text, for
    /// the diff and patch media types.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport or status failure.
    pub async fn get_text(&self, path: &str, accept: &str) -> Result<String, PlatformError> {
        let builder = self
            .request_accepting(
                reqwest::Method::GET,
                &format!("{}{path}", self.api_base),
                accept,
            )
            .await?;
        let response = builder.send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(PlatformError::Status {
                status: status.as_u16(),
                body: truncate(&body),
            });
        }
        Ok(body)
    }

    /// `GET` a REST path authenticated as the App itself (JWT), not as an
    /// installation. Only `/app` and `/app/installations` need this.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] when the client has no App
    /// credentials, otherwise as [`Self::get`].
    pub async fn get_as_app(&self, path: &str) -> Result<Value, PlatformError> {
        let jwt = self
            .auth
            .app_jwt()
            .ok_or_else(|| PlatformError::Auth("no App credentials configured".to_owned()))??;
        let builder = self
            .http
            .get(format!("{}{path}", self.api_base))
            .bearer_auth(jwt.expose_secret())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        self.send(builder).await
    }

    /// `GET` every page of a REST list, to its end: a page shorter than
    /// `PAGE_SIZE` (100) is the last.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport, status or decode failure, and
    /// [`PlatformError::TooMany`] when the list goes on past `MAX_PAGES`
    /// (100) full pages.
    pub async fn get_all(&self, path: &str) -> Result<Vec<Value>, PlatformError> {
        self.get_pages(path, false).await
    }

    /// [`Self::get_all`] authenticated as the App itself (JWT), for
    /// `/app/installations`.
    ///
    /// # Errors
    ///
    /// As [`Self::get_all`], and [`PlatformError::Auth`] when the client has
    /// no App credentials.
    pub async fn get_all_as_app(&self, path: &str) -> Result<Vec<Value>, PlatformError> {
        self.get_pages(path, true).await
    }

    async fn get_pages(&self, path: &str, as_app: bool) -> Result<Vec<Value>, PlatformError> {
        let mut items = Vec::new();
        let separator = if path.contains('?') { '&' } else { '?' };
        // One page past the cap tells a list of exactly the cap, which ends
        // there, from one that goes on.
        for page in 1..=MAX_PAGES + 1 {
            let paged = format!("{path}{separator}per_page={PAGE_SIZE}&page={page}");
            let page_items = if as_app {
                self.get_as_app(&paged).await?
            } else {
                self.get(&paged).await?
            };
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

    /// `POST` JSON to a REST path.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport, status or decode failure.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, PlatformError> {
        let builder = self
            .request(reqwest::Method::POST, &format!("{}{path}", self.api_base))
            .await?;
        self.send(builder.json(body)).await
    }

    /// `PATCH` JSON to a REST path.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport, status or decode failure.
    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, PlatformError> {
        let builder = self
            .request(reqwest::Method::PATCH, &format!("{}{path}", self.api_base))
            .await?;
        self.send(builder.json(body)).await
    }

    /// Runs a GraphQL query and returns `data`.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Decode`] when the response carries `errors`.
    #[instrument(skip_all)]
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value, PlatformError> {
        let builder = self
            .request(reqwest::Method::POST, &self.graphql_url)
            .await?;
        let response = self
            .send(builder.json(&json!({"query": query, "variables": variables})))
            .await?;
        if let Some(errors) = response.get("errors").filter(|e| !e.is_null()) {
            return Err(PlatformError::Decode(format!("GraphQL errors: {errors}")));
        }
        Ok(response.get("data").cloned().unwrap_or(Value::Null))
    }

    /// Creates a check run, `in_progress` or `queued` as `run.status` says,
    /// and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    pub async fn create_check_run(
        &self,
        owner: &str,
        repo: &str,
        run: &NewCheckRun<'_>,
    ) -> Result<u64, PlatformError> {
        let mut body = json!({
            "name": run.name,
            "head_sha": run.head_sha,
            "status": run.status,
            "details_url": run.details_url,
            "external_id": run.external_id,
        });
        if let Some((title, summary)) = run.output
            && let Some(fields) = body.as_object_mut()
        {
            fields.insert(
                "output".to_owned(),
                json!({"title": title, "summary": summary}),
            );
        }
        let created = self
            .post(&format!("/repos/{owner}/{repo}/check-runs"), &body)
            .await?;
        created
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::Decode("check run without id".to_owned()))
    }

    /// Moves a queued check run to in progress, linked to the run (#262).
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    pub async fn start_check_run(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
        details_url: &str,
    ) -> Result<(), PlatformError> {
        self.patch(
            &format!("/repos/{owner}/{repo}/check-runs/{check_run_id}"),
            &json!({
                "status": "in_progress",
                "details_url": details_url,
                "output": {"title": "In progress", "summary": format!("Run: {details_url}")},
            }),
        )
        .await?;
        Ok(())
    }

    /// Completes a check run.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the fields of the check-run API"
    )]
    pub async fn complete_check_run(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
        conclusion: &str,
        title: &str,
        summary: &str,
        details_url: &str,
    ) -> Result<(), PlatformError> {
        self.patch(
            &format!("/repos/{owner}/{repo}/check-runs/{check_run_id}"),
            &json!({
                "status": "completed",
                "conclusion": conclusion,
                "details_url": details_url,
                "output": {"title": title, "summary": summary},
            }),
        )
        .await?;
        Ok(())
    }
}
