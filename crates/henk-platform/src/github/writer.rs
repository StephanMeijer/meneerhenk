//! The GitHub writer.

use henk_domain::allowlist::Platform;
use henk_domain::marker::{Marker, MarkerKind};
use henk_domain::review::{CheckConclusion, CommitSha, ReviewOutcome};
use serde_json::{Value, json};
use tracing::{debug, instrument};

use crate::error::PlatformError;
use crate::github::api::GitHubApi;
use crate::writer::{
    DiffSide, ExistingFinding, ExistingSummary, FilePatch, PlatformWriter, PostedComment,
    PullRequestInfo, PullRequestState, ReviewHandle, ReviewTarget,
};

/// Name of the check run (§3.3).
pub const CHECK_NAME: &str = "Meneer Henk";

/// Writes to GitHub through the REST and GraphQL APIs as the App.
#[derive(Debug)]
pub struct GitHubWriter {
    api: GitHubApi,
    bot_login: String,
}

impl GitHubWriter {
    /// Builds a writer. `bot_login` is the App's login, such as
    /// `meneer-henk[bot]`, used to recognise Henk's own comments.
    #[must_use]
    pub fn new(api: GitHubApi, bot_login: impl Into<String>) -> Self {
        Self {
            api,
            bot_login: bot_login.into(),
        }
    }

    /// The underlying API client.
    #[must_use]
    pub fn api(&self) -> &GitHubApi {
        &self.api
    }

    fn is_henk(&self, comment: &Value) -> bool {
        let login = comment
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or("");
        login == self.bot_login || Marker::is_present(body_of(comment))
    }
}

fn posted(value: &Value) -> Result<PostedComment, PlatformError> {
    let id = value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| PlatformError::Decode("comment without id".to_owned()))?;
    Ok(PostedComment {
        id: id.to_string(),
        node_id: value
            .get("node_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: value
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    })
}

fn body_of(value: &Value) -> &str {
    value.get("body").and_then(Value::as_str).unwrap_or("")
}

#[async_trait::async_trait]
impl PlatformWriter for GitHubWriter {
    fn platform(&self) -> Platform {
        Platform::GitHub
    }

    async fn diff(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        base_ref: &str,
    ) -> Result<Vec<FilePatch>, PlatformError> {
        // Three dots: the change since the merge base, which is what the
        // pull request shows.
        let text = self
            .api
            .get_text(
                &format!(
                    "/repos/{}/compare/{base_ref}...{}",
                    target.repo.path(),
                    commit.as_str()
                ),
                "application/vnd.github.diff",
            )
            .await?;
        Ok(henk_domain::diff::split_unified(&text))
    }

    async fn pull_request(&self, target: &ReviewTarget) -> Result<PullRequestInfo, PlatformError> {
        let pr = self
            .api
            .get(&format!(
                "/repos/{}/pulls/{}",
                target.repo.path(),
                target.number
            ))
            .await?;
        let head = pr
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .and_then(|sha| CommitSha::parse(sha).ok())
            .ok_or_else(|| PlatformError::Decode("pull request without head sha".to_owned()))?;
        Ok(PullRequestInfo {
            title: pr
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            head,
            base_ref: pr
                .pointer("/base/ref")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            draft: pr.get("draft").and_then(Value::as_bool).unwrap_or(false),
            state: if pr.get("state").and_then(Value::as_str) == Some("open") {
                PullRequestState::Open
            } else {
                PullRequestState::Closed
            },
        })
    }

    #[instrument(skip_all, fields(repo = %target.repo.path(), number = target.number))]
    async fn start_review(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        run_link: &str,
    ) -> Result<Option<ReviewHandle>, PlatformError> {
        let id = self
            .api
            .create_check_run(
                target.repo.owner(),
                target.repo.name(),
                CHECK_NAME,
                commit.as_str(),
                run_link,
                run_link,
            )
            .await?;
        Ok(Some(ReviewHandle(id.to_string())))
    }

    async fn acknowledge(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        is_review_comment: bool,
    ) -> Result<(), PlatformError> {
        let path = if is_review_comment {
            format!(
                "/repos/{}/pulls/comments/{comment_id}/reactions",
                target.repo.path()
            )
        } else {
            format!(
                "/repos/{}/issues/comments/{comment_id}/reactions",
                target.repo.path()
            )
        };
        self.api.post(&path, &json!({"content": "eyes"})).await?;
        Ok(())
    }

    async fn existing_findings(
        &self,
        target: &ReviewTarget,
    ) -> Result<Vec<ExistingFinding>, PlatformError> {
        let comments = self
            .api
            .get_all(&format!(
                "/repos/{}/pulls/{}/comments",
                target.repo.path(),
                target.number
            ))
            .await?;
        let threads = self
            .api
            .review_threads(target.repo.owner(), target.repo.name(), target.number)
            .await?;

        let mut findings = Vec::new();
        for comment in &comments {
            if comment.get("in_reply_to_id").is_some_and(|v| !v.is_null()) {
                continue;
            }
            let Some(marker) = Marker::parse(body_of(comment)) else {
                continue;
            };
            if marker.kind.is_some_and(|k| k != MarkerKind::Finding) {
                continue;
            }
            let Some(id) = comment.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let thread = threads
                .iter()
                .find(|t| t.comments.iter().any(|c| c.database_id == id));
            let answered_by_person = thread.is_some_and(|t| {
                t.comments
                    .iter()
                    .any(|c| c.database_id != id && !c.is_bot && c.author != self.bot_login)
            });
            findings.push(ExistingFinding {
                comment_id: id.to_string(),
                node_id: comment
                    .get("node_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                path: comment
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                line: comment
                    .get("line")
                    .and_then(Value::as_u64)
                    .and_then(|l| u32::try_from(l).ok()),
                body: body_of(comment).to_owned(),
                marker,
                resolved: thread.is_some_and(|t| t.resolved),
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
        let comments = self
            .api
            .get_all(&format!(
                "/repos/{}/issues/{}/comments",
                target.repo.path(),
                target.number
            ))
            .await?;
        Ok(comments
            .iter()
            .filter(|c| self.is_henk(c))
            .filter_map(|c| {
                let marker = Marker::parse(body_of(c))?;
                if !matches!(marker.kind, Some(MarkerKind::Summary | MarkerKind::Failure)) {
                    return None;
                }
                Some(ExistingSummary {
                    comment_id: c.get("id")?.as_u64()?.to_string(),
                    node_id: c.get("node_id").and_then(Value::as_str).map(str::to_owned),
                    marker,
                    folded: false,
                })
            })
            .collect())
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
        let created = self
            .api
            .post(
                &format!(
                    "/repos/{}/pulls/{}/comments",
                    target.repo.path(),
                    target.number
                ),
                &json!({
                    "body": body,
                    "commit_id": commit.as_str(),
                    "path": path,
                    "line": line,
                    "side": match side { DiffSide::Left => "LEFT", DiffSide::Right => "RIGHT" },
                }),
            )
            .await?;
        posted(&created)
    }

    async fn update_finding(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        body: &str,
    ) -> Result<(), PlatformError> {
        self.api
            .patch(
                &format!("/repos/{}/pulls/comments/{comment_id}", target.repo.path()),
                &json!({"body": body}),
            )
            .await?;
        Ok(())
    }

    async fn post_comment(
        &self,
        target: &ReviewTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let created = self
            .api
            .post(
                &format!(
                    "/repos/{}/issues/{}/comments",
                    target.repo.path(),
                    target.number
                ),
                &json!({"body": body}),
            )
            .await?;
        posted(&created)
    }

    async fn reply(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        is_review_comment: bool,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        if is_review_comment {
            let created = self
                .api
                .post(
                    &format!(
                        "/repos/{}/pulls/{}/comments/{comment_id}/replies",
                        target.repo.path(),
                        target.number
                    ),
                    &json!({"body": body}),
                )
                .await?;
            posted(&created)
        } else {
            self.post_comment(target, body).await
        }
    }

    async fn resolve_finding(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
    ) -> Result<(), PlatformError> {
        let threads = self
            .api
            .review_threads(target.repo.owner(), target.repo.name(), target.number)
            .await?;
        let thread = threads
            .iter()
            .find(|t| {
                t.comments
                    .iter()
                    .any(|c| c.database_id.to_string() == comment_id)
            })
            .ok_or_else(|| {
                PlatformError::Decode(format!("no review thread holds comment {comment_id}"))
            })?;
        if thread.resolved {
            return Ok(());
        }
        self.api.resolve_review_thread(&thread.id).await
    }

    async fn fold_summary(
        &self,
        _target: &ReviewTarget,
        summary: &ExistingSummary,
    ) -> Result<(), PlatformError> {
        match &summary.node_id {
            Some(node_id) => self.api.minimize_comment(node_id).await,
            None => Err(PlatformError::Decode("summary without node id".to_owned())),
        }
    }

    async fn fold_finding(
        &self,
        _target: &ReviewTarget,
        finding: &ExistingFinding,
    ) -> Result<(), PlatformError> {
        match &finding.node_id {
            Some(node_id) => self.api.minimize_comment(node_id).await,
            None => Err(PlatformError::Decode("finding without node id".to_owned())),
        }
    }

    async fn finish_review(
        &self,
        target: &ReviewTarget,
        _commit: &CommitSha,
        handle: Option<&ReviewHandle>,
        outcome: &ReviewOutcome,
        run_link: &str,
    ) -> Result<(), PlatformError> {
        let Some(ReviewHandle(id)) = handle else {
            return Ok(());
        };
        let id: u64 = id
            .parse()
            .map_err(|_| PlatformError::Decode(format!("check run id {id:?}")))?;
        let conclusion = match outcome.check_conclusion() {
            CheckConclusion::Success => "success",
            CheckConclusion::Neutral => "neutral",
            CheckConclusion::Failure => "failure",
        };
        self.api
            .complete_check_run(
                target.repo.owner(),
                target.repo.name(),
                id,
                conclusion,
                &outcome.headline(),
                &format!("{}\n\nRun: {run_link}", outcome.summary()),
                run_link,
            )
            .await
    }
}
