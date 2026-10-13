//! The GitLab writer. Every write is an MCP call to the configured
//! write-mode `gitlab-mcp` session, made by Henk's code with arguments it
//! constructs. No model ever holds this session.

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::diff::FileStatus;
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_mcp::McpSession;
use serde_json::{Value, json};
use tracing::{instrument, warn};

use crate::error::PlatformError;
use crate::gitlab::rest::GitLabRest;
use crate::retry::{RetryPolicy, create_once, with_retry};
use crate::writer::{
    FilePatch, PlatformWriter, PostedComment, PullRequestInfo, PullRequestState, ReviewHandle,
    ReviewTarget,
};

/// Name of the commit status (§3.3).
pub const STATUS_NAME: &str = "Meneer Henk";

/// The account the token belongs to (§2: by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BotUser {
    pub(crate) id: u64,
    pub(crate) username: String,
}

/// Writes to GitLab through a write-mode MCP session.
pub struct GitLabWriter {
    session: Arc<dyn McpSession>,
    username: String,
    rest: Option<GitLabRest>,
    bot: tokio::sync::OnceCell<BotUser>,
    /// How a call is tried again when GitLab fails for a moment (#338).
    retry: RetryPolicy,
}

impl std::fmt::Debug for GitLabWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLabWriter")
            .field("session", &self.session.alias())
            .field("username", &self.username)
            .field("rest", &self.rest)
            .finish_non_exhaustive()
    }
}

impl GitLabWriter {
    /// Builds a writer. `username` is Henk's GitLab username, used to
    /// recognise his notes.
    #[must_use]
    pub fn new(session: Arc<dyn McpSession>, username: impl Into<String>) -> Self {
        Self {
            session,
            username: username.into(),
            rest: None,
            bot: tokio::sync::OnceCell::new(),
            retry: RetryPolicy::default(),
        }
    }

    /// The same writer, trying again by `retry` (tests use a fast one).
    #[must_use]
    pub const fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Adds the REST reads and the git credential pushing (§3.5) needs.
    /// Without them the writer reviews but does not push.
    #[must_use]
    pub fn with_rest(mut self, rest: GitLabRest) -> Self {
        self.rest = Some(rest);
        self
    }

    /// Whether this writer can push.
    #[must_use]
    pub fn can_push(&self) -> bool {
        self.rest.is_some()
    }

    pub(crate) fn rest(&self) -> Result<&GitLabRest, PlatformError> {
        self.rest.as_ref().ok_or_else(|| {
            PlatformError::Auth(
                "pushing to GitLab needs the GitLab token, which is not set".to_owned(),
            )
        })
    }

    /// The account the token belongs to, read once.
    pub(crate) async fn bot_user(&self) -> Result<&BotUser, PlatformError> {
        let rest = self.rest()?;
        self.bot
            .get_or_try_init(|| async {
                let user = rest.get("/user").await?;
                let id = user
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| PlatformError::Decode("user without id".to_owned()))?;
                let username = user
                    .get("username")
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| PlatformError::Decode("user without username".to_owned()))?
                    .to_owned();
                Ok(BotUser { id, username })
            })
            .await
    }

    /// Calls a tool and parses its text as JSON. A result the server flags
    /// as an error becomes [`PlatformError::ToolFailed`]. A tool that is
    /// safe to repeat is tried again when GitLab fails for a moment (#338);
    /// one that creates something is called once. Only the merge request
    /// notes are then looked up and tried again ([`Self::create_note_once`]);
    /// a work item note, a merge request and a work item are not yet
    /// (#412), so one 5xx fails them.
    pub(crate) async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, PlatformError> {
        if !repeatable(tool) {
            return self.call_tool_once(tool, arguments).await;
        }
        with_retry(self.retry, tool, || {
            self.call_tool_once(tool, arguments.clone())
        })
        .await
    }

    /// Posts a merge request note once (#338): `tool` with `arguments`,
    /// and when it fails in a way another try might not, the merge
    /// request's discussions are read first. A note by Henk with exactly
    /// `body` is the one that landed before GitLab failed; each body
    /// carries the run's marker, so an exact match is his own.
    async fn create_note_once(
        &self,
        target: &ReviewTarget,
        tool: &str,
        arguments: Value,
        body: &str,
    ) -> Result<Value, PlatformError> {
        create_once(
            self.retry,
            tool,
            || self.call_tool_once(tool, arguments.clone()),
            || async {
                let discussions = self
                    .call_tool_all(
                        "mr_discussions",
                        Self::base_args(target),
                        &["items", "discussions"],
                    )
                    .await?;
                Ok(discussions
                    .iter()
                    .flat_map(|d| {
                        d.get("notes")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .rev()
                    .find(|note| {
                        note.get("body").and_then(Value::as_str) == Some(body)
                            && note.pointer("/author/username").and_then(Value::as_str)
                                == Some(self.username.as_str())
                    })
                    .cloned())
            },
        )
        .await
    }

    async fn call_tool_once(&self, tool: &str, arguments: Value) -> Result<Value, PlatformError> {
        let outcome = self.session.call_tool(tool, arguments).await?;
        if outcome.is_error {
            return Err(PlatformError::ToolFailed {
                tool: tool.to_owned(),
                message: outcome.text,
            });
        }
        if let Some(structured) = outcome.structured {
            return Ok(structured);
        }
        Ok(serde_json::from_str(&outcome.text).unwrap_or(Value::String(outcome.text)))
    }

    pub(crate) fn base_args(target: &ReviewTarget) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("project_id".into(), json!(target.repo.path()));
        map.insert("merge_request_iid".into(), json!(target.number.to_string()));
        map
    }

    pub(crate) async fn merge_request(
        &self,
        target: &ReviewTarget,
    ) -> Result<Value, PlatformError> {
        self.call_tool("get_merge_request", Value::Object(Self::base_args(target)))
            .await
    }

    /// Every item of a paged list tool, to its end: `per_page` 100, until a
    /// page has fewer than 100 items. A page is an array or an object
    /// holding the array under one of `keys`. A list that goes on past 100
    /// full pages is [`PlatformError::TooMany`], never a quietly partial one (#113).
    pub(crate) async fn call_tool_all(
        &self,
        tool: &str,
        arguments: serde_json::Map<String, Value>,
        keys: &[&str],
    ) -> Result<Vec<Value>, PlatformError> {
        const PAGE_SIZE: usize = 100;
        // Lists are read to their end (#113); this only stops a runaway.
        const MAX_PAGES: usize = 100;
        let mut all = Vec::new();
        // One page past the cap tells a list of exactly the cap, which ends
        // there, from one that goes on.
        for page in 1..=MAX_PAGES + 1 {
            let mut args = arguments.clone();
            args.insert("per_page".into(), json!(PAGE_SIZE));
            args.insert("page".into(), json!(page));
            let items = match self.call_tool(tool, Value::Object(args)).await? {
                Value::Array(items) => items,
                Value::Object(mut map) => keys
                    .iter()
                    .find_map(|key| match map.remove(*key) {
                        Some(Value::Array(items)) => Some(items),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            if page > MAX_PAGES && !items.is_empty() {
                break;
            }
            let count = items.len();
            all.extend(items);
            if count < PAGE_SIZE {
                return Ok(all);
            }
        }
        Err(PlatformError::TooMany {
            what: format!("{tool} with {}", Value::Object(arguments)),
            limit: MAX_PAGES * PAGE_SIZE,
        })
    }
}

/// Whether calling `tool` twice is no different from calling it once: a
/// read, or a write that sets rather than adds. A reaction is not: GitLab
/// refuses an award it already has.
fn repeatable(tool: &str) -> bool {
    tool.starts_with("get_")
        || tool.starts_with("list_")
        || matches!(
            tool,
            "mr_discussions"
                | "update_work_item"
                | "convert_work_item_type"
                | "create_commit_status"
        )
}

pub(crate) fn note_id(note: &Value) -> Option<String> {
    match note.get("id") {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

#[async_trait::async_trait]
impl PlatformWriter for GitLabWriter {
    fn platform(&self) -> Platform {
        Platform::GitLab
    }

    async fn diff(
        &self,
        target: &ReviewTarget,
        _commit: &CommitSha,
        _base_ref: &str,
    ) -> Result<Vec<FilePatch>, PlatformError> {
        // GitLab serves the merge request's current diff; a merge request
        // under review at an older commit is superseded anyway.
        let value = self
            .call_tool(
                "get_merge_request_diffs",
                Value::Object(Self::base_args(target)),
            )
            .await?;
        let items = match value {
            Value::Array(items) => items,
            Value::Object(ref map) => map
                .get("changes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(items
            .iter()
            .filter_map(|item| {
                let get = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
                let flag = |key: &str| item.get(key).and_then(Value::as_bool).unwrap_or(false);
                let status = if flag("new_file") {
                    FileStatus::Added
                } else if flag("deleted_file") {
                    FileStatus::Removed
                } else if flag("renamed_file") {
                    FileStatus::Renamed
                } else {
                    FileStatus::Modified
                };
                let old_path = get("old_path").filter(|_| status != FileStatus::Added);
                let new_path = get("new_path").filter(|_| status != FileStatus::Removed);
                if old_path.is_none() && new_path.is_none() {
                    return None;
                }
                Some(FilePatch {
                    old_path,
                    new_path,
                    status,
                    patch: get("diff").unwrap_or_default(),
                })
            })
            .collect())
    }

    async fn pull_request(&self, target: &ReviewTarget) -> Result<PullRequestInfo, PlatformError> {
        let mr = self.merge_request(target).await?;
        let head = mr
            .get("sha")
            .and_then(Value::as_str)
            .or_else(|| mr.pointer("/diff_refs/head_sha").and_then(Value::as_str))
            .and_then(|sha| CommitSha::parse(sha).ok())
            .ok_or_else(|| PlatformError::Decode("merge request without head sha".to_owned()))?;
        Ok(PullRequestInfo {
            title: mr
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            head,
            base_ref: mr
                .get("target_branch")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            draft: mr.get("draft").and_then(Value::as_bool).unwrap_or(false)
                || mr
                    .get("work_in_progress")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            state: if mr.get("state").and_then(Value::as_str) == Some("opened") {
                PullRequestState::Open
            } else {
                PullRequestState::Closed
            },
        })
    }

    #[instrument(skip_all, fields(project = %target.repo.path(), iid = target.number))]
    async fn start_review(
        &self,
        target: &ReviewTarget,
        _commit: &CommitSha,
        _run_link: &str,
        _queued: Option<&ReviewHandle>,
    ) -> Result<Option<ReviewHandle>, PlatformError> {
        let mut args = Self::base_args(target);
        args.insert("name".into(), json!("eyes"));
        if let Err(error) = self
            .call_tool("create_merge_request_emoji_reaction", Value::Object(args))
            .await
        {
            // Reacting twice is an error on GitLab; it is not worth failing a review over.
            warn!(%error, "could not react to the merge request");
        }
        Ok(None)
    }

    async fn acknowledge(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        _is_review_comment: bool,
    ) -> Result<(), PlatformError> {
        let mut args = Self::base_args(target);
        args.insert("note_id".into(), json!(comment_id));
        args.insert("name".into(), json!("eyes"));
        self.call_tool(
            "create_merge_request_note_emoji_reaction",
            Value::Object(args),
        )
        .await?;
        Ok(())
    }

    async fn post_comment(
        &self,
        target: &ReviewTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let mut args = Self::base_args(target);
        args.insert("body".into(), json!(body));
        let created = self
            .create_note_once(
                target,
                "create_merge_request_note",
                Value::Object(args),
                body,
            )
            .await?;
        let id =
            note_id(&created).ok_or_else(|| PlatformError::Decode("note without id".to_owned()))?;
        Ok(PostedComment {
            id,
            node_id: None,
            url: String::new(),
        })
    }

    async fn reply(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        is_review_comment: bool,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        if is_review_comment {
            // `comment_id` is the discussion id for diff notes (see events.rs).
            let mut args = Self::base_args(target);
            args.insert("discussion_id".into(), json!(comment_id));
            args.insert("body".into(), json!(body));
            let created = self
                .create_note_once(
                    target,
                    "create_merge_request_discussion_note",
                    Value::Object(args),
                    body,
                )
                .await?;
            let id = note_id(&created)
                .ok_or_else(|| PlatformError::Decode("reply without id".to_owned()))?;
            return Ok(PostedComment {
                id,
                node_id: Some(comment_id.to_owned()),
                url: String::new(),
            });
        }
        self.post_comment(target, body).await
    }

    async fn finish_review(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        _handle: Option<&ReviewHandle>,
        outcome: &ReviewOutcome,
        run_link: &str,
    ) -> Result<(), PlatformError> {
        // Always success: the status must never block a pipeline (§8.2).
        let args = json!({
            "project_id": target.repo.path(),
            "sha": commit.as_str(),
            "state": "success",
            "name": STATUS_NAME,
            "description": outcome.headline(),
            "target_url": run_link,
        });
        self.call_tool("create_commit_status", args).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::needless_pass_by_value
    )]

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_mcp::testing::FakeServer;
    use rmcp::model::{CallToolResult, ContentBlock};

    use super::*;

    fn target() -> ReviewTarget {
        ReviewTarget {
            repo: RepoRef::parse(Platform::GitLab, "9xxlab/tools/cli").unwrap(),
            number: 5,
        }
    }

    fn text(value: Value) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(value.to_string())])
    }

    /// One discussion by a person, numbered by its page and place.
    fn discussion(page: u64, i: u64) -> Value {
        json!({"id": format!("d-{page}-{i}"), "notes": [
            {"id": page * 1000 + i, "body": "looks fine", "author": {"username": "alice"}}
        ]})
    }

    /// `mr_discussions` with 100 discussions on every page before `last`,
    /// one on page `last`, and nothing past it.
    fn paged_discussions(last: u64) -> FakeServer {
        let tools = vec![FakeServer::tool("mr_discussions", "", &[])];
        FakeServer::new(tools, move |_, args| {
            let page = args["page"].as_u64().unwrap_or(0);
            let count = match page.cmp(&last) {
                std::cmp::Ordering::Less => 100,
                std::cmp::Ordering::Equal => 1,
                std::cmp::Ordering::Greater => 0,
            };
            text(Value::Array(
                (0..count).map(|i| discussion(page, i)).collect(),
            ))
        })
    }

    async fn writer(fake: &FakeServer) -> GitLabWriter {
        GitLabWriter::new(Arc::new(fake.connect("gitlab-write").await), "meneerhenk")
    }

    /// Every page of `mr_discussions`, as a listing that pages reads them.
    async fn all_discussions(fake: &FakeServer) -> Result<Vec<Value>, PlatformError> {
        writer(fake)
            .await
            .call_tool_all(
                "mr_discussions",
                GitLabWriter::base_args(&target()),
                &["items", "discussions"],
            )
            .await
    }

    #[tokio::test]
    async fn discussions_on_the_eleventh_page_are_read() {
        let fake = paged_discussions(11);
        let discussions = all_discussions(&fake).await.unwrap();
        assert_eq!(discussions.len(), 1001);
        assert_eq!(discussions[1000]["id"], "d-11-0");
        assert_eq!(fake.calls().len(), 11);
    }

    #[tokio::test]
    async fn a_discussion_list_of_exactly_the_cap_is_read_whole() {
        let tools = vec![FakeServer::tool("mr_discussions", "", &[])];
        let fake = FakeServer::new(tools, |_, args| {
            let page = args["page"].as_u64().unwrap_or(0);
            let count = if page > 100 { 0 } else { 100 };
            text(Value::Array(
                (0..count).map(|i| discussion(page, i)).collect(),
            ))
        });
        let discussions = all_discussions(&fake).await.unwrap();
        assert_eq!(
            discussions.len(),
            10_000,
            "10,000 discussions is the cap, not past it: the list is whole"
        );
        assert_eq!(fake.calls().len(), 101, "one empty page shows the end");
    }

    #[tokio::test]
    async fn a_discussion_list_past_the_cap_is_an_error() {
        let fake = paged_discussions(u64::MAX);
        let result = all_discussions(&fake).await;
        assert!(
            matches!(result, Err(PlatformError::TooMany { limit: 10_000, .. })),
            "{result:?}"
        );
        assert_eq!(fake.calls().len(), 101, "stops one page past the cap");
    }
}
