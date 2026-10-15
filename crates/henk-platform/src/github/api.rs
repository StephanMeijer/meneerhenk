//! The GitHub REST and GraphQL calls Henk needs that the MCP server does not
//! provide safely: review comments at a commit, check runs, reactions,
//! comment edits, review-thread state, and minimising comments.

use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use tracing::instrument;

use henk_domain::allowlist::RepoRef;

use crate::error::{PlatformError, success_text, truncate};
use crate::github::app::{GitHubAuth, Installations, Scope};
use crate::retry::{RetryPolicy, with_retry};

const JSON: &str = "application/vnd.github+json";

/// A thin client bound to one authentication. With App authentication it
/// acts for one installation: build it once, then take
/// [`Self::for_repository`] for each repository's work.
#[derive(Debug)]
pub struct GitHubApi {
    http: reqwest::Client,
    api_base: String,
    graphql_url: String,
    auth: GitHubAuth,
    /// How reads and updates are tried again when GitHub fails for a
    /// moment (#338).
    retry: RetryPolicy,
}

const USER_AGENT: &str = "meneer-henk (https://github.com/StephanMeijer/meneerhenk)";

/// Entries per page of every paged read.
const PAGE_SIZE: usize = 100;

/// Pages read before a list counts as too long: 10,000 entries. Lists are
/// read to their end (#113); this cap only stops a runaway, and reaching it
/// is [`PlatformError::TooMany`], never a quietly partial list.
const MAX_PAGES: usize = 100;

/// `value` as a query-string value: every byte but an unreserved one is
/// percent-encoded, so `+`, `&`, `%`, `#` and a space survive the trip.
pub(crate) use crate::encode::percent_encode as encode_query_value;

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
            retry: RetryPolicy::default(),
        })
    }

    /// The same client, trying again by `retry` (tests use a fast one).
    #[must_use]
    pub const fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// How this client tries again.
    #[must_use]
    pub const fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    fn scoped(&self, scope: Scope) -> Self {
        Self {
            http: self.http.clone(),
            api_base: self.api_base.clone(),
            graphql_url: self.graphql_url.clone(),
            auth: self.auth.scoped(scope),
            retry: self.retry,
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
        self.auth
            .installation(&self.http, &self.api_base, self.retry)
            .await
    }

    async fn request_accepting(
        &self,
        method: reqwest::Method,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::RequestBuilder, PlatformError> {
        let token = self
            .auth
            .bearer(&self.http, &self.api_base, self.retry)
            .await?;
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(token.expose_secret())
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28"))
    }

    /// One request, sent once: the body of a 2xx answer, or the status
    /// error with what GitHub said and how long it asked to wait.
    async fn once(
        &self,
        method: reqwest::Method,
        url: &str,
        accept: &str,
        body: Option<&Value>,
    ) -> Result<String, PlatformError> {
        let builder = self.request_accepting(method, url, accept).await?;
        let builder = match body {
            Some(body) => builder.json(body),
            None => builder,
        };
        let response = builder.send().await?;
        success_text(response).await
    }

    /// The request, tried again when GitHub fails for a moment. Only for
    /// requests that are safe to repeat.
    async fn retried(
        &self,
        method: reqwest::Method,
        path: &str,
        accept: &str,
        body: Option<&Value>,
    ) -> Result<String, PlatformError> {
        let url = format!("{}{path}", self.api_base);
        let what = format!("{method} {path}");
        with_retry(self.retry, &what, || {
            self.once(method.clone(), &url, accept, body)
        })
        .await
    }

    /// The token git uses to clone and push as the App: the same cached
    /// installation token the API calls use. Never logged, never in argv.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when no token can be minted.
    pub async fn git_token(&self) -> Result<secrecy::SecretString, PlatformError> {
        self.auth
            .bearer(&self.http, &self.api_base, self.retry)
            .await
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
        decode(&self.retried(reqwest::Method::GET, path, JSON, None).await?)
    }

    /// `GET` a REST path with a specific `Accept` and the body as text, for
    /// the diff and patch media types.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport or status failure.
    pub async fn get_text(&self, path: &str, accept: &str) -> Result<String, PlatformError> {
        self.retried(reqwest::Method::GET, path, accept, None).await
    }

    /// `GET` a REST path as text, keeping only its last `max_bytes`, cut
    /// at a line: for a CI log, which can run to hundreds of megabytes
    /// (#420). A redirect is followed, without the token on another host.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport or status failure.
    pub async fn get_text_tail(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<crate::ci::CiLog, PlatformError> {
        let url = format!("{}{path}", self.api_base);
        with_retry(self.retry, &format!("GET {path}"), || async {
            let mut response = self
                .request_accepting(reqwest::Method::GET, &url, JSON)
                .await?
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let headers = response.headers().clone();
                let text = response.text().await?;
                return Err(PlatformError::status(status.as_u16(), &text, &headers));
            }
            let mut tail = crate::ci::Tail::new(max_bytes);
            while let Some(chunk) = response.chunk().await? {
                tail.push(&chunk);
            }
            Ok(tail.finish())
        })
        .await
    }

    /// `GET` a REST path authenticated as the App itself (JWT), not as an
    /// installation. Only `/app` and `/app/installations` need this.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] when the client has no App
    /// credentials, otherwise as [`Self::get`].
    pub async fn get_as_app(&self, path: &str) -> Result<Value, PlatformError> {
        let url = format!("{}{path}", self.api_base);
        let text = with_retry(self.retry, &format!("GET {path}"), || async {
            let jwt = self
                .auth
                .app_jwt()
                .ok_or_else(|| PlatformError::Auth("no App credentials configured".to_owned()))??;
            let response = self
                .http
                .get(&url)
                .bearer_auth(jwt.expose_secret())
                .header("Accept", JSON)
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .await?;
            success_text(response).await
        })
        .await?;
        decode(&text)
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
        self.get_pages(path, false, None).await
    }

    /// [`Self::get_all`] for a list GitHub wraps in an object, such as
    /// `{"check_runs": [...]}`: the entries under `key` of every page.
    ///
    /// # Errors
    ///
    /// As [`Self::get_all`].
    pub async fn get_all_under(&self, path: &str, key: &str) -> Result<Vec<Value>, PlatformError> {
        self.get_pages(path, false, Some(key)).await
    }

    /// [`Self::get_all`] authenticated as the App itself (JWT), for
    /// `/app/installations`.
    ///
    /// # Errors
    ///
    /// As [`Self::get_all`], and [`PlatformError::Auth`] when the client has
    /// no App credentials.
    pub async fn get_all_as_app(&self, path: &str) -> Result<Vec<Value>, PlatformError> {
        self.get_pages(path, true, None).await
    }

    async fn get_pages(
        &self,
        path: &str,
        as_app: bool,
        key: Option<&str>,
    ) -> Result<Vec<Value>, PlatformError> {
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
            let page_items = match key {
                Some(key) => page_items.get(key).cloned().unwrap_or(Value::Null),
                None => page_items,
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
        let url = format!("{}{path}", self.api_base);
        decode(
            &self
                .once(reqwest::Method::POST, &url, JSON, Some(body))
                .await?,
        )
    }

    /// `POST` JSON, tried again when GitHub fails for a moment: only for a
    /// create GitHub does not repeat, such as a reaction it already has.
    ///
    /// # Errors
    ///
    /// As [`Self::post`], once the attempts run out.
    pub async fn post_retrying(&self, path: &str, body: &Value) -> Result<Value, PlatformError> {
        decode(
            &self
                .retried(reqwest::Method::POST, path, JSON, Some(body))
                .await?,
        )
    }

    /// Posts a comment with `body` to `post_path` once (#338). When the post
    /// fails in a way another try might not, the comments at `list_path`
    /// are read first: one by `author` with exactly `body`, and for which
    /// `matches` holds, is the post that landed before GitHub failed. Henk's
    /// bodies carry the run's marker, so an exact match is his own.
    ///
    /// # Errors
    ///
    /// As [`crate::retry::create_once`].
    pub async fn create_comment_once(
        &self,
        post_path: &str,
        list_path: &str,
        body: &str,
        author: &str,
        matches: impl Fn(&Value) -> bool,
    ) -> Result<Value, PlatformError> {
        let request = json!({"body": body});
        crate::retry::create_once(
            self.retry,
            &format!("POST {post_path}"),
            || self.post(post_path, &request),
            || async {
                let comments = self.get_all(list_path).await?;
                Ok(comments.into_iter().rev().find(|comment| {
                    comment.get("body").and_then(Value::as_str) == Some(body)
                        && comment.pointer("/user/login").and_then(Value::as_str) == Some(author)
                        && matches(comment)
                }))
            },
        )
        .await
    }

    /// `PATCH` JSON to a REST path.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport, status or decode failure.
    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, PlatformError> {
        decode(
            &self
                .retried(reqwest::Method::PATCH, path, JSON, Some(body))
                .await?,
        )
    }

    /// Runs a GraphQL query and returns `data`.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Decode`] when the response carries `errors`.
    #[instrument(skip_all)]
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value, PlatformError> {
        let response = decode(
            &self
                .once(
                    reqwest::Method::POST,
                    &self.graphql_url,
                    JSON,
                    Some(&json!({"query": query, "variables": variables})),
                )
                .await?,
        )?;
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
        let path = format!("/repos/{owner}/{repo}/check-runs");
        let created = crate::retry::create_once(
            self.retry,
            &format!("POST {path}"),
            || self.post(&path, &body),
            || self.find_check_run(owner, repo, run),
        )
        .await?;
        created
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::Decode("check run without id".to_owned()))
    }

    /// The check run `run` describes, if a create that failed made it
    /// anyway: on its commit, by its name and its `external_id`, which is
    /// the run's own link.
    async fn find_check_run(
        &self,
        owner: &str,
        repo: &str,
        run: &NewCheckRun<'_>,
    ) -> Result<Option<Value>, PlatformError> {
        let found = self
            .get(&format!(
                "/repos/{owner}/{repo}/commits/{}/check-runs?check_name={}&filter=all&per_page=100",
                run.head_sha,
                encode_query_value(run.name)
            ))
            .await?;
        Ok(found
            .get("check_runs")
            .and_then(Value::as_array)
            .and_then(|runs| {
                runs.iter()
                    .find(|r| r.get("external_id").and_then(Value::as_str) == Some(run.external_id))
                    .cloned()
            }))
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

/// A 2xx body as JSON; an empty one is `null`.
fn decode(body: &str) -> Result<Value, PlatformError> {
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(body)
        .map_err(|e| PlatformError::Decode(format!("{e}: {}", truncate(body))))
}

#[cfg(test)]
mod tests {
    use super::encode_query_value;

    #[test]
    fn a_query_value_keeps_only_unreserved_bytes() {
        assert_eq!(encode_query_value("feat/a+b&c"), "feat%2Fa%2Bb%26c");
        assert_eq!(encode_query_value("100% a#b"), "100%25%20a%23b");
        assert_eq!(encode_query_value("v1.2_x-y~z"), "v1.2_x-y~z");
        assert_eq!(encode_query_value("é"), "%C3%A9");
    }
}
