//! The GitHub REST and GraphQL calls Henk needs that the MCP server does not
//! provide safely: review comments at a commit, check runs, reactions,
//! comment edits, review-thread state, and minimising comments.

use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use tracing::instrument;

use crate::error::{PlatformError, truncate};
use crate::github::app::GitHubAuth;

/// A thin client bound to one authentication.
#[derive(Debug)]
pub struct GitHubApi {
    http: reqwest::Client,
    api_base: String,
    graphql_url: String,
    auth: GitHubAuth,
}

/// A review thread with the ids of its comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewThread {
    /// GraphQL node id of the thread.
    pub id: String,
    /// Whether it is resolved.
    pub resolved: bool,
    /// The comments in the thread.
    pub comments: Vec<ThreadComment>,
}

/// One comment in a review thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadComment {
    /// REST id.
    pub database_id: u64,
    /// Author login.
    pub author: String,
    /// Whether the author is a bot or App.
    pub is_bot: bool,
}

const USER_AGENT: &str = "meneer-henk (https://github.com/StephanMeijer/meneerhenk)";

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
            .timeout(std::time::Duration::from_secs(60))
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

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, PlatformError> {
        let token = self.auth.bearer(&self.http, &self.api_base).await?;
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(token.expose_secret())
            .header("Accept", "application/vnd.github+json")
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

    /// `GET` every page of a REST list (up to ten pages of 100).
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on transport, status or decode failure.
    pub async fn get_all(&self, path: &str) -> Result<Vec<Value>, PlatformError> {
        let mut items = Vec::new();
        for page in 1..=10 {
            let separator = if path.contains('?') { '&' } else { '?' };
            let page_items = self
                .get(&format!("{path}{separator}per_page=100&page={page}"))
                .await?;
            let Some(array) = page_items.as_array() else {
                break;
            };
            let count = array.len();
            items.extend(array.iter().cloned());
            if count < 100 {
                break;
            }
        }
        Ok(items)
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

    /// Creates a check run and returns its id.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    pub async fn create_check_run(
        &self,
        owner: &str,
        repo: &str,
        name: &str,
        head_sha: &str,
        details_url: &str,
        external_id: &str,
    ) -> Result<u64, PlatformError> {
        let created = self
            .post(
                &format!("/repos/{owner}/{repo}/check-runs"),
                &json!({
                    "name": name,
                    "head_sha": head_sha,
                    "status": "in_progress",
                    "details_url": details_url,
                    "external_id": external_id,
                }),
            )
            .await?;
        created
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::Decode("check run without id".to_owned()))
    }

    /// Completes a check run.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    #[allow(clippy::too_many_arguments)]
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

    /// Review threads of a pull request with resolution state.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    pub async fn review_threads(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<ReviewThread>, PlatformError> {
        const QUERY: &str = r"
query($owner: String!, $repo: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          comments(first: 100) {
            nodes { databaseId author { login __typename } }
          }
        }
      }
    }
  }
}";
        let mut threads = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..10 {
            let data = self
                .graphql(
                    QUERY,
                    json!({"owner": owner, "repo": repo, "number": number, "after": after}),
                )
                .await?;
            let connection = data
                .pointer("/repository/pullRequest/reviewThreads")
                .cloned()
                .unwrap_or(Value::Null);
            for node in connection
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let comments = node
                    .pointer("/comments/nodes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|c| {
                        Some(ThreadComment {
                            database_id: c.get("databaseId")?.as_u64()?,
                            author: c
                                .pointer("/author/login")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            is_bot: c.pointer("/author/__typename").and_then(Value::as_str)
                                == Some("Bot"),
                        })
                    })
                    .collect();
                threads.push(ReviewThread {
                    id: node
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    resolved: node
                        .get("isResolved")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    comments,
                });
            }
            let has_next = connection
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_next {
                break;
            }
            after = connection
                .pointer("/pageInfo/endCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        Ok(threads)
    }

    /// Minimises a comment as outdated.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    pub async fn minimize_comment(&self, node_id: &str) -> Result<(), PlatformError> {
        const MUTATION: &str = r"
mutation($id: ID!) {
  minimizeComment(input: {subjectId: $id, classifier: OUTDATED}) {
    minimizedComment { isMinimized }
  }
}";
        self.graphql(MUTATION, json!({"id": node_id})).await?;
        Ok(())
    }
}
