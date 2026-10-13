//! What Henk writes to a platform, behind one trait per platform.

use henk_domain::allowlist::Platform;
use henk_domain::review::{CommitSha, ReviewOutcome};

use crate::error::PlatformError;

pub use henk_domain::review::ReviewTarget;

/// Open, closed or merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    /// Open.
    Open,
    /// Closed without being merged.
    Closed,
    /// Merged: the work landed (#413).
    Merged,
}

/// What a review needs to know about its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestInfo {
    /// Title.
    pub title: String,
    /// Head commit.
    pub head: CommitSha,
    /// Base branch name.
    pub base_ref: String,
    /// Whether it is a draft.
    pub draft: bool,
    /// Open, closed or merged.
    pub state: PullRequestState,
}

pub use henk_domain::diff::FilePatch;

/// A comment Henk posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedComment {
    /// The platform's numeric or string id.
    pub id: String,
    /// GraphQL node id, where the platform has one.
    pub node_id: Option<String>,
    /// Link to the comment.
    pub url: String,
}

/// What `start_review` hands back so `finish_review` can close it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewHandle(pub String);

/// Everything Henk writes to a pull/merge request.
#[async_trait::async_trait]
pub trait PlatformWriter: Send + Sync {
    /// The platform.
    fn platform(&self) -> Platform;

    /// Reads the target.
    async fn pull_request(&self, target: &ReviewTarget) -> Result<PullRequestInfo, PlatformError>;

    /// The diff of the target at `commit` against `base_ref`, one patch per
    /// file. A review reads its own commit, not the current head.
    async fn diff(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        base_ref: &str,
    ) -> Result<Vec<FilePatch>, PlatformError>;

    /// Shows that a review waits for a slot (#262): GitHub, a check run
    /// *queued*, the grey one, with `title` and `summary`, linked to `link`.
    /// Returns its handle for `start_review` and `finish_review`. Nothing
    /// by default: on GitLab a pending commit status could block a merge,
    /// and Henk never blocks one (§8.2).
    async fn queue_review(
        &self,
        _target: &ReviewTarget,
        _commit: &CommitSha,
        _title: &str,
        _summary: &str,
        _link: &str,
    ) -> Result<Option<ReviewHandle>, PlatformError> {
        Ok(None)
    }

    /// Marks a review as started (GitHub: check run in progress; GitLab:
    /// award emoji). `queued` is the check `queue_review` opened, which
    /// then moves to in progress instead of a second one being made.
    /// Returns a handle for `finish_review` when there is one.
    async fn start_review(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        run_link: &str,
        queued: Option<&ReviewHandle>,
    ) -> Result<Option<ReviewHandle>, PlatformError>;

    /// Reacts 👀 to a comment (§3.1).
    async fn acknowledge(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        is_review_comment: bool,
    ) -> Result<(), PlatformError>;

    /// Posts a conversation comment (summary, greeting, failure).
    async fn post_comment(
        &self,
        target: &ReviewTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError>;

    /// Replies to a comment. On GitHub a review comment gets a threaded
    /// reply; a conversation comment gets a conversation comment.
    async fn reply(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        is_review_comment: bool,
        body: &str,
    ) -> Result<PostedComment, PlatformError>;

    /// Closes the review: check conclusion or commit status (§3.3).
    async fn finish_review(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        handle: Option<&ReviewHandle>,
        outcome: &ReviewOutcome,
        run_link: &str,
    ) -> Result<(), PlatformError>;
}
