//! GitHub and GitLab.
//!
//! Webhook verification and parsing into platform-neutral events, and the
//! writers that post findings, summaries, replies, check runs and statuses.
//! Everything a model reads goes through MCP, but for a pull request's CI
//! (#420), read through a [`CiReader`]; everything Henk writes goes
//! through a [`PlatformWriter`], which is code, not prompt (spec §8.4).

pub mod ci;
mod encode;
pub mod error;
pub mod github;
pub mod gitlab;
pub mod issue;
pub mod push;
pub mod retry;
pub mod webhook;
pub mod writer;

pub use ci::{CiAnnotation, CiConclusion, CiJob, CiJobId, CiLog, CiReader, CiStatus};
pub use error::PlatformError;
pub use issue::{CreatedSubIssue, IssueInfo, IssueRelation, IssueTarget, IssueUpdate, IssueWriter};
pub use writer::{
    FilePatch, PlatformWriter, PostedComment, PullRequestInfo, PullRequestState, ReviewHandle,
    ReviewTarget,
};
