//! What Henk writes to a platform, behind one trait per platform.

use henk_domain::allowlist::Platform;
use henk_domain::marker::Marker;
use henk_domain::review::{CommitSha, ReviewOutcome};

use crate::error::PlatformError;

pub use henk_domain::review::ReviewTarget;

/// Open or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestState {
    /// Open.
    Open,
    /// Closed or merged.
    Closed,
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
    /// Open or closed.
    pub state: PullRequestState,
}

pub use henk_domain::diff::{DiffSide, FilePatch};

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

/// One of Henk's findings already on the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingFinding {
    /// Comment id.
    pub comment_id: String,
    /// GraphQL node id, where the platform has one.
    pub node_id: Option<String>,
    /// File path.
    pub path: String,
    /// Current line, or `None` when the comment is outdated.
    pub line: Option<u32>,
    /// Visible text, marker included.
    pub body: String,
    /// The parsed marker.
    pub marker: Marker,
    /// Whether its thread is resolved.
    pub resolved: bool,
    /// Whether a person replied in its thread.
    pub answered_by_person: bool,
}

/// One of Henk's earlier summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSummary {
    /// Comment id.
    pub comment_id: String,
    /// GraphQL node id, where the platform has one.
    pub node_id: Option<String>,
    /// The parsed marker.
    pub marker: Marker,
    /// Whether it is already folded.
    pub folded: bool,
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

    /// Marks a review as started (GitHub: check run in progress; GitLab:
    /// award emoji). Returns a handle for `finish_review` when there is one.
    async fn start_review(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        run_link: &str,
    ) -> Result<Option<ReviewHandle>, PlatformError>;

    /// Reacts 👀 to a comment (§3.1).
    async fn acknowledge(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        is_review_comment: bool,
    ) -> Result<(), PlatformError>;

    /// Henk's findings on the target, with thread state.
    async fn existing_findings(
        &self,
        target: &ReviewTarget,
    ) -> Result<Vec<ExistingFinding>, PlatformError>;

    /// Henk's summaries on the target.
    async fn existing_summaries(
        &self,
        target: &ReviewTarget,
    ) -> Result<Vec<ExistingSummary>, PlatformError>;

    /// Posts a line comment at `commit`.
    async fn post_finding(
        &self,
        target: &ReviewTarget,
        commit: &CommitSha,
        path: &str,
        line: u32,
        side: DiffSide,
        body: &str,
    ) -> Result<PostedComment, PlatformError>;

    /// Rewrites a line comment.
    async fn update_finding(
        &self,
        target: &ReviewTarget,
        comment_id: &str,
        body: &str,
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

    /// Folds an earlier summary as outdated (§3.3).
    async fn fold_summary(
        &self,
        target: &ReviewTarget,
        summary: &ExistingSummary,
    ) -> Result<(), PlatformError>;

    /// Folds a finding whose thread is resolved (§3.3).
    async fn fold_finding(
        &self,
        target: &ReviewTarget,
        finding: &ExistingFinding,
    ) -> Result<(), PlatformError>;

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
