//! What a planner writes to an issue, behind one trait per platform (§4).

use henk_domain::allowlist::{Platform, RepoRef};

use crate::error::PlatformError;
use crate::writer::PostedComment;

pub use henk_domain::plan::IssueTarget;

/// What a planner needs to know about an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueInfo {
    /// The platform's global id, when it has one apart from the number.
    pub id: Option<u64>,
    /// Number (GitLab: iid).
    pub number: u64,
    /// Title.
    pub title: String,
    /// Description, plan section included.
    pub body: String,
    /// Whether it is open.
    pub open: bool,
    /// Whether it is really a pull/merge request.
    pub is_pull_request: bool,
    /// Current labels.
    pub labels: Vec<String>,
    /// Link.
    pub url: String,
}

/// How two issues relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueRelation {
    /// The other issue is the parent of this one.
    Parent,
    /// The other issue is a sub-issue of this one.
    SubIssue,
    /// This issue blocks the other.
    Blocks,
    /// This issue is blocked by the other.
    BlockedBy,
    /// Related, nothing more.
    RelatesTo,
}

impl IssueRelation {
    /// Parses the words a tool accepts.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "parent" => Self::Parent,
            "sub_issue" | "sub-issue" | "child" => Self::SubIssue,
            "blocks" => Self::Blocks,
            "blocked_by" | "blocked-by" | "is_blocked_by" => Self::BlockedBy,
            "relates_to" | "relates-to" | "related" => Self::RelatesTo,
            _ => return None,
        })
    }
}

/// Fields of an issue a planner may change. `None` leaves a field alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IssueUpdate {
    /// New title.
    pub title: Option<String>,
    /// New description.
    pub body: Option<String>,
    /// New full label set.
    pub labels: Option<Vec<String>>,
    /// New issue type (GitHub issue types; GitLab `issue_type`).
    pub issue_type: Option<String>,
}

/// Everything a planner writes to an issue tracker.
#[async_trait::async_trait]
pub trait IssueWriter: Send + Sync {
    /// The platform.
    fn platform(&self) -> Platform;

    /// Reads an issue.
    async fn issue(&self, target: &IssueTarget) -> Result<IssueInfo, PlatformError>;

    /// Changes fields of an issue.
    async fn update_issue(
        &self,
        target: &IssueTarget,
        update: IssueUpdate,
    ) -> Result<(), PlatformError>;

    /// The labels that exist in the repository.
    async fn repo_labels(&self, repo: &RepoRef) -> Result<Vec<String>, PlatformError>;

    /// Creates an issue.
    async fn create_issue(
        &self,
        repo: &RepoRef,
        title: &str,
        body: &str,
    ) -> Result<IssueInfo, PlatformError>;

    /// Registers a relationship between `target` and `other` in the same repository.
    async fn link_issues(
        &self,
        target: &IssueTarget,
        relation: IssueRelation,
        other: u64,
    ) -> Result<(), PlatformError>;

    /// Posts a comment on the issue.
    async fn comment(
        &self,
        target: &IssueTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError>;
}
