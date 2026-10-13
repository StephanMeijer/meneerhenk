//! The GitHub writer.

use henk_domain::allowlist::Platform;
use henk_domain::review::{CheckConclusion, CommitSha, ReviewOutcome};
use serde_json::{Value, json};
use tracing::instrument;

use crate::error::PlatformError;
use crate::github::api::{GitHubApi, NewCheckRun};
use crate::writer::{
    FilePatch, PlatformWriter, PostedComment, PullRequestInfo, PullRequestState, ReviewHandle,
    ReviewTarget,
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

    /// The App's login.
    #[must_use]
    pub fn bot_login(&self) -> &str {
        &self.bot_login
    }

    /// The underlying API client.
    #[must_use]
    pub fn api(&self) -> &GitHubApi {
        &self.api
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
            } else if crate::github::merged(&pr) {
                PullRequestState::Merged
            } else {
                PullRequestState::Closed
            },
        })
    }

    #[instrument(skip_all, fields(repo = %target.repo.path(), number = target.number))]
    async fn queue_review(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        title: &str,
        summary: &str,
        link: &str,
    ) -> Result<Option<ReviewHandle>, PlatformError> {
        let id = self
            .api
            .create_check_run(
                target.repo.owner(),
                target.repo.name(),
                &NewCheckRun {
                    name: CHECK_NAME,
                    head_sha: commit.as_str(),
                    status: "queued",
                    details_url: link,
                    external_id: link,
                    output: Some((title, summary)),
                },
            )
            .await?;
        Ok(Some(ReviewHandle(id.to_string())))
    }

    #[instrument(skip_all, fields(repo = %target.repo.path(), number = target.number))]
    async fn start_review(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        run_link: &str,
        queued: Option<&ReviewHandle>,
    ) -> Result<Option<ReviewHandle>, PlatformError> {
        if let Some(handle) = queued {
            let id: u64 = handle
                .0
                .parse()
                .map_err(|_| PlatformError::Decode(format!("check run id {:?}", handle.0)))?;
            self.api
                .start_check_run(target.repo.owner(), target.repo.name(), id, run_link)
                .await?;
            return Ok(Some(handle.clone()));
        }
        let id = self
            .api
            .create_check_run(
                target.repo.owner(),
                target.repo.name(),
                &NewCheckRun {
                    name: CHECK_NAME,
                    head_sha: commit.as_str(),
                    status: "in_progress",
                    details_url: run_link,
                    external_id: run_link,
                    output: None,
                },
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
        // GitHub answers a repeat with the reaction it already has.
        self.api
            .post_retrying(&path, &json!({"content": "eyes"}))
            .await?;
        Ok(())
    }

    async fn post_comment(
        &self,
        target: &ReviewTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let path = format!(
            "/repos/{}/issues/{}/comments",
            target.repo.path(),
            target.number
        );
        let created = self
            .api
            .create_comment_once(&path, &path, body, self.bot_login(), |_| true)
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
            let path = format!(
                "/repos/{}/pulls/{}/comments/{comment_id}/replies",
                target.repo.path(),
                target.number
            );
            let created = self
                .api
                .create_comment_once(
                    &path,
                    &format!(
                        "/repos/{}/pulls/{}/comments",
                        target.repo.path(),
                        target.number
                    ),
                    body,
                    self.bot_login(),
                    |comment| {
                        comment
                            .get("in_reply_to_id")
                            .and_then(Value::as_u64)
                            .is_some_and(|id| id.to_string() == comment_id)
                    },
                )
                .await?;
            posted(&created)
        } else {
            self.post_comment(target, body).await
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
