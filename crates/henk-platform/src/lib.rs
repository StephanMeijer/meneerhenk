//! GitHub and GitLab.
//!
//! Webhook verification and parsing into platform-neutral events, and the
//! writers that post findings, summaries, replies, check runs and statuses.
//! Everything a model reads goes through MCP; everything Henk writes goes
//! through a [`PlatformWriter`], which is code, not prompt (spec §8.4).

pub mod address;
pub mod error;
pub mod github;
pub mod gitlab;
pub mod issue;
pub mod webhook;
pub mod writer;

pub use error::PlatformError;
pub use issue::{IssueInfo, IssueRelation, IssueTarget, IssueUpdate, IssueWriter};
pub use writer::{
    DiffSide, ExistingFinding, ExistingSummary, FilePatch, PlatformWriter, PostedComment,
    PullRequestInfo, PullRequestState, ReviewHandle, ReviewTarget,
};
