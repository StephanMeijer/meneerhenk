//! GitHub and GitLab.
//!
//! Webhook verification and parsing into platform-neutral events, and the
//! writers that post findings, summaries, replies, check runs and statuses.
//! Everything a model reads goes through MCP; everything Henk writes goes
//! through a [`PlatformWriter`], which is code, not prompt (spec §8.4).

pub mod error;
pub mod events;
pub mod github;
pub mod webhook;
pub mod writer;

pub use error::PlatformError;
pub use events::{CommentKind, IncomingEvent, PullRequestAction, Sender};
pub use writer::{
    DiffSide, ExistingFinding, ExistingSummary, PlatformWriter, PostedComment, PullRequestInfo,
    PullRequestState, ReviewHandle, ReviewTarget,
};
