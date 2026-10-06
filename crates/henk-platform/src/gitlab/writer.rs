//! The GitLab writer. Every write is an MCP call to the configured
//! write-mode `gitlab-mcp` session, made by Henk's code with arguments it
//! constructs. No model ever holds this session.

use std::sync::Arc;

use henk_domain::allowlist::Platform;
use henk_domain::diff::FileStatus;
use henk_domain::marker::{Marker, MarkerKind};
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_mcp::McpSession;
use serde_json::{Value, json};
use tracing::{debug, instrument, warn};

use crate::error::PlatformError;
use crate::gitlab::rest::GitLabRest;
use crate::writer::{
    DiffSide, ExistingFinding, ExistingSummary, FilePatch, PlatformWriter, PostedComment,
    PullRequestInfo, PullRequestState, ReviewHandle, ReviewTarget,
};

/// Name of the commit status (§3.3).
pub const STATUS_NAME: &str = "Meneer Henk";

/// Prefix of a folded summary note.
pub const OUTDATED_PREFIX: &str = "*Outdated.*";

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

/// The diff refs a positioned note needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffRefs {
    base: String,
    start: String,
    head: String,
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
        }
    }

    /// Adds the REST reads and the git credential an address run (§3.5)
    /// needs. Without them the writer reviews but does not address.
    #[must_use]
    pub fn with_rest(mut self, rest: GitLabRest) -> Self {
        self.rest = Some(rest);
        self
    }

    /// Whether this writer can do an address run.
    #[must_use]
    pub fn can_address(&self) -> bool {
        self.rest.is_some()
    }

    pub(crate) fn rest(&self) -> Result<&GitLabRest, PlatformError> {
        self.rest.as_ref().ok_or_else(|| {
            PlatformError::Auth(
                "GitLab address runs need the GitLab token, which is not set".to_owned(),
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
    /// as an error becomes [`PlatformError::ToolFailed`].
    pub(crate) async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, PlatformError> {
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

    fn diff_refs(merge_request: &Value) -> Result<DiffRefs, PlatformError> {
        let refs = merge_request
            .get("diff_refs")
            .ok_or_else(|| PlatformError::Decode("merge request without diff_refs".to_owned()))?;
        let get = |key: &str| {
            refs.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| PlatformError::Decode(format!("diff_refs without {key}")))
        };
        Ok(DiffRefs {
            base: get("base_sha")?,
            start: get("start_sha")?,
            head: get("head_sha")?,
        })
    }

    /// Every item of a paged list tool: `per_page` 100, pages 1 to 10,
    /// until a page has fewer than 100 items. A page is an array or an
    /// object holding the array under one of `keys`.
    pub(crate) async fn call_tool_all(
        &self,
        tool: &str,
        arguments: serde_json::Map<String, Value>,
        keys: &[&str],
    ) -> Result<Vec<Value>, PlatformError> {
        const PAGE_SIZE: usize = 100;
        const MAX_PAGES: u32 = 10;
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
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
            let count = items.len();
            all.extend(items);
            if count < PAGE_SIZE {
                break;
            }
        }
        Ok(all)
    }

    /// All discussions of a merge request, across pages.
    pub(crate) async fn discussions(
        &self,
        target: &ReviewTarget,
    ) -> Result<Vec<Value>, PlatformError> {
        self.call_tool_all(
            "mr_discussions",
            Self::base_args(target),
            &["items", "discussions"],
        )
        .await
    }

    fn is_henk_note(&self, note: &Value) -> bool {
        note.pointer("/author/username").and_then(Value::as_str) == Some(self.username.as_str())
            || Marker::is_present(note_body(note))
    }
}

pub(crate) fn note_body(note: &Value) -> &str {
    note.get("body").and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn note_id(note: &Value) -> Option<String> {
    match note.get("id") {
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

pub(crate) fn is_system(note: &Value) -> bool {
    note.get("system").and_then(Value::as_bool).unwrap_or(false)
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

    async fn existing_findings(
        &self,
        target: &ReviewTarget,
    ) -> Result<Vec<ExistingFinding>, PlatformError> {
        let mut findings = Vec::new();
        for discussion in self.discussions(target).await? {
            let notes = discussion
                .get("notes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let Some(first) = notes.first() else { continue };
            if is_system(first) || first.get("position").is_none_or(Value::is_null) {
                continue;
            }
            let Some(marker) = Marker::parse(note_body(first)) else {
                continue;
            };
            if marker.kind.is_some_and(|k| k != MarkerKind::Finding) {
                continue;
            }
            let Some(id) = note_id(first) else { continue };
            let position = first.get("position").cloned().unwrap_or(Value::Null);
            let line = position
                .get("new_line")
                .and_then(Value::as_u64)
                .or_else(|| position.get("old_line").and_then(Value::as_u64))
                .and_then(|l| u32::try_from(l).ok());
            let answered_by_person = notes
                .iter()
                .skip(1)
                .any(|n| !is_system(n) && !self.is_henk_note(n));
            findings.push(ExistingFinding {
                comment_id: id,
                node_id: discussion
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                path: position
                    .get("new_path")
                    .or_else(|| position.get("old_path"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                line,
                body: note_body(first).to_owned(),
                marker,
                resolved: first
                    .get("resolved")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                answered_by_person,
            });
        }
        debug!(count = findings.len(), "existing findings");
        Ok(findings)
    }

    async fn existing_summaries(
        &self,
        target: &ReviewTarget,
    ) -> Result<Vec<ExistingSummary>, PlatformError> {
        let mut summaries = Vec::new();
        for discussion in self.discussions(target).await? {
            let notes = discussion
                .get("notes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let Some(first) = notes.first() else { continue };
            if is_system(first)
                || !first.get("position").is_none_or(Value::is_null)
                || !self.is_henk_note(first)
            {
                continue;
            }
            let Some(marker) = Marker::parse(note_body(first)) else {
                continue;
            };
            if !matches!(marker.kind, Some(MarkerKind::Summary | MarkerKind::Failure)) {
                continue;
            }
            let Some(id) = note_id(first) else { continue };
            summaries.push(ExistingSummary {
                comment_id: id,
                node_id: discussion
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                marker,
                folded: note_body(first).trim_start().starts_with(OUTDATED_PREFIX),
            });
        }
        Ok(summaries)
    }

    async fn post_finding(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        path: &str,
        line: u32,
        side: DiffSide,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let mr = self.merge_request(target).await?;
        let refs = Self::diff_refs(&mr)?;
        if refs.head != commit.as_str() {
            warn!(head = %refs.head, reviewed = %commit, "merge request moved on; positioning on its current head");
        }
        let mut position = serde_json::Map::new();
        position.insert("base_sha".into(), json!(refs.base));
        position.insert("start_sha".into(), json!(refs.start));
        position.insert("head_sha".into(), json!(refs.head));
        position.insert("position_type".into(), json!("text"));
        position.insert("new_path".into(), json!(path));
        position.insert("old_path".into(), json!(path));
        let line_key = match side {
            DiffSide::Right => "new_line",
            DiffSide::Left => "old_line",
        };
        position.insert(line_key.into(), json!(line));
        let position = Value::Object(position);
        let mut args = Self::base_args(target);
        args.insert("body".into(), json!(body));
        args.insert("position".into(), position);
        let created = self
            .call_tool("create_merge_request_thread", Value::Object(args))
            .await?;
        let first = created.pointer("/notes/0").cloned().unwrap_or(Value::Null);
        let id = note_id(&first)
            .or_else(|| note_id(&created))
            .ok_or_else(|| PlatformError::Decode("thread without a note id".to_owned()))?;
        Ok(PostedComment {
            id,
            node_id: created.get("id").and_then(Value::as_str).map(str::to_owned),
            url: String::new(),
        })
    }

    async fn update_finding(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        body: &str,
    ) -> Result<(), PlatformError> {
        let mut args = Self::base_args(target);
        args.insert("note_id".into(), json!(comment_id));
        args.insert("body".into(), json!(body));
        self.call_tool("update_merge_request_note", Value::Object(args))
            .await?;
        Ok(())
    }

    async fn resolve_finding(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
    ) -> Result<(), PlatformError> {
        let discussion_id = self
            .discussions(target)
            .await?
            .iter()
            .find(|d| {
                d.get("notes")
                    .and_then(Value::as_array)
                    .and_then(|notes| notes.first())
                    .and_then(note_id)
                    .is_some_and(|id| id == comment_id)
            })
            .and_then(|d| d.get("id").and_then(Value::as_str).map(str::to_owned))
            .ok_or_else(|| {
                PlatformError::Decode(format!("no discussion starts with note {comment_id}"))
            })?;
        let mut args = Self::base_args(target);
        args.insert("discussion_id".into(), json!(discussion_id));
        args.insert("resolved".into(), json!(true));
        self.call_tool("resolve_merge_request_thread", Value::Object(args))
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
            .call_tool("create_merge_request_note", Value::Object(args))
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
                .call_tool("create_merge_request_discussion_note", Value::Object(args))
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

    async fn fold_summary(
        &self,
        target: &ReviewTarget,
        summary: &ExistingSummary,
    ) -> Result<(), PlatformError> {
        if summary.folded {
            return Ok(());
        }
        let body = format!("{OUTDATED_PREFIX}\n\n{}", summary.marker.render());
        self.update_finding(target, &summary.comment_id, &body)
            .await
    }

    async fn fold_finding(
        &self,
        _target: &ReviewTarget,
        _finding: &ExistingFinding,
    ) -> Result<(), PlatformError> {
        // GitLab collapses resolved threads itself; there is nothing to fold.
        Ok(())
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
