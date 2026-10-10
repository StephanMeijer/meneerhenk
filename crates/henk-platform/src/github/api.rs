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
    /// Whether the line it is on changed since.
    pub outdated: bool,
    /// The file it is on.
    pub path: Option<String>,
    /// The line it is on, when it still has one.
    pub line: Option<u32>,
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
    /// The author's stable account id (§2), when GitHub gives one.
    pub author_id: Option<u64>,
    /// The comment text.
    pub body: String,
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
        let mut items = Vec::new();
        let separator = if path.contains('?') { '&' } else { '?' };
        // One page past the cap tells a list of exactly the cap, which ends
        // there, from one that goes on.
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

    /// Review threads of a pull request with resolution state, every
    /// thread and every comment in it, read to the end (#113). Address runs
    /// give the model a thread's whole conversation, so a long thread's
    /// newest replies must not be cut off.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure, and [`PlatformError::TooMany`]
    /// past `MAX_PAGES` (100) pages of threads or of one thread's comments.
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
          isOutdated
          path
          line
          comments(first: 100) {
            pageInfo { hasNextPage endCursor }
            nodes {
              databaseId
              body
              author { login __typename ... on User { databaseId } ... on Bot { databaseId } }
            }
          }
        }
      }
    }
  }
}";
        let mut threads = Vec::new();
        // Threads with more comments than the first page, and where to go on.
        let mut unfinished: Vec<(usize, String)> = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
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
                if let Some(cursor) = next_cursor(node.get("comments").unwrap_or(&Value::Null)) {
                    unfinished.push((threads.len(), cursor));
                }
                threads.push(parse_thread(node));
            }
            if let Some(cursor) = next_cursor(&connection) {
                after = Some(cursor);
                continue;
            }
            for (index, cursor) in unfinished {
                if let Some(thread) = threads.get_mut(index) {
                    let rest = self.thread_comments(&thread.id, cursor).await?;
                    thread.comments.extend(rest);
                }
            }
            return Ok(threads);
        }
        Err(PlatformError::TooMany {
            what: format!("the review threads of {owner}/{repo}#{number}"),
            limit: MAX_PAGES * PAGE_SIZE,
        })
    }

    /// The comments of one review thread after `after`, to the end.
    async fn thread_comments(
        &self,
        thread: &str,
        after: String,
    ) -> Result<Vec<ThreadComment>, PlatformError> {
        const QUERY: &str = r"
query($id: ID!, $after: String) {
  node(id: $id) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          databaseId
          body
          author { login __typename ... on User { databaseId } ... on Bot { databaseId } }
        }
      }
    }
  }
}";
        let mut comments = Vec::new();
        let mut after = Some(after);
        for _ in 0..MAX_PAGES {
            let data = self
                .graphql(QUERY, json!({"id": thread, "after": after}))
                .await?;
            let connection = data
                .pointer("/node/comments")
                .cloned()
                .unwrap_or(Value::Null);
            comments.extend(parse_comments(&connection));
            match next_cursor(&connection) {
                Some(cursor) => after = Some(cursor),
                None => return Ok(comments),
            }
        }
        Err(PlatformError::TooMany {
            what: format!("the comments of review thread {thread}"),
            limit: MAX_PAGES * PAGE_SIZE,
        })
    }

    /// Resolves a review thread.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] on failure.
    pub async fn resolve_review_thread(&self, thread_id: &str) -> Result<(), PlatformError> {
        const MUTATION: &str = r"
mutation($id: ID!) {
  resolveReviewThread(input: {threadId: $id}) {
    thread { isResolved }
  }
}";
        self.graphql(MUTATION, json!({"id": thread_id})).await?;
        Ok(())
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

/// One `reviewThreads` node.
/// The cursor of the next page of a GraphQL connection, when there is one.
fn next_cursor(connection: &Value) -> Option<String> {
    let more = connection
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    more.then(|| {
        connection
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
    .flatten()
}

fn parse_thread(node: &Value) -> ReviewThread {
    let comments = parse_comments(node.get("comments").unwrap_or(&Value::Null));
    ReviewThread {
        id: node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        resolved: node
            .get("isResolved")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        outdated: node
            .get("isOutdated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        path: node.get("path").and_then(Value::as_str).map(str::to_owned),
        line: node
            .get("line")
            .and_then(Value::as_u64)
            .and_then(|l| u32::try_from(l).ok()),
        comments,
    }
}

/// The comments of one page of a thread's `comments` connection.
fn parse_comments(connection: &Value) -> Vec<ThreadComment> {
    connection
        .get("nodes")
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
                is_bot: c.pointer("/author/__typename").and_then(Value::as_str) == Some("Bot"),
                author_id: c.pointer("/author/databaseId").and_then(Value::as_u64),
                body: c
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect()
}
