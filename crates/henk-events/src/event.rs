//! The event model: what a hook received, as every listener sees it.

use henk_domain::allowlist::RepoRef;
use henk_domain::plan::IssueTarget;
use henk_domain::review::{CommitSha, ReviewTarget};
use henk_domain::run::{EventId, RunId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who caused the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    /// Login or username.
    pub login: String,
    /// Whether the platform marks the account as a bot.
    pub is_bot: bool,
}

/// What happened to a pull/merge request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestAction {
    /// Opened, or marked ready for review.
    Opened,
    /// Reopened.
    Reopened,
    /// New commits were pushed.
    Synchronized,
}

/// Where a comment sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentKind {
    /// A conversation comment on the pull request or issue.
    Conversation,
    /// A review comment on a diff line.
    Review {
        /// The comment this replies to, if any.
        in_reply_to: Option<String>,
    },
}

/// Where an event came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum EventSource {
    /// A GitHub webhook delivery.
    GitHubWebhook {
        /// The `X-GitHub-Delivery` id.
        delivery: String,
    },
    /// A GitLab webhook.
    GitLabWebhook {
        /// The `X-Gitlab-Event` header.
        event: String,
    },
    /// The HTTP API, on behalf of a Team Lead.
    Api {
        /// Who the request runs for, as a stable id.
        requester: Option<String>,
    },
    /// The web dashboard, by someone signed in (#69).
    Dashboard {
        /// Who asked: `github:<user id>`, never a display name (§2).
        requester: String,
    },
    /// Henk's MCP server, by a client with a write token (#249).
    Mcp {
        /// Who asked: `mcp:<token name>`.
        requester: String,
    },
}

impl EventSource {
    /// A short name for records and logs.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::GitHubWebhook { .. } => "github_webhook",
            Self::GitLabWebhook { .. } => "gitlab_webhook",
            Self::Api { .. } => "api",
            Self::Dashboard { .. } => "dashboard",
            Self::Mcp { .. } => "mcp",
        }
    }

    /// Who asked, when the source knows: the API's configured requester
    /// or the person signed in to the dashboard.
    #[must_use]
    pub fn requester(&self) -> Option<&str> {
        match self {
            Self::Api { requester } => requester.as_deref(),
            Self::Dashboard { requester } | Self::Mcp { requester } => Some(requester),
            Self::GitHubWebhook { .. } | Self::GitLabWebhook { .. } => None,
        }
    }
}

/// What happened, platform-neutral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// A pull/merge request changed.
    PullRequest {
        /// Repository.
        repo: RepoRef,
        /// Number (GitLab: iid).
        number: u64,
        /// What happened.
        action: PullRequestAction,
        /// Head commit.
        head: CommitSha,
        /// Whether it is a draft.
        draft: bool,
        /// Who did it.
        sender: Sender,
    },
    /// Someone commented on an open pull/merge request.
    Comment {
        /// Repository.
        repo: RepoRef,
        /// Pull/merge request number.
        number: u64,
        /// The comment text.
        body: String,
        /// The platform's id of the comment.
        comment_id: String,
        /// Where it sits.
        kind: CommentKind,
        /// Who wrote it.
        sender: Sender,
    },
    /// A review was asked for directly (API, later Discord).
    ReviewRequested {
        /// The pull/merge request.
        target: ReviewTarget,
        /// The commit to review; `None` means the current head.
        commit: Option<CommitSha>,
        /// Who asked, as a stable id.
        requester: Option<String>,
    },
    /// A plan was asked for directly (API, later Discord).
    PlanRequested {
        /// The issue.
        target: IssueTarget,
        /// A note from the requester.
        note: Option<String>,
        /// Who asked, as a stable id.
        requester: Option<String>,
    },
    /// Addressing a pull request's review feedback was asked for (§3.5).
    AddressRequested {
        /// The pull request.
        target: ReviewTarget,
        /// A note from the requester.
        note: Option<String>,
        /// Who asked, as a stable id.
        requester: Option<String>,
    },
    /// A delivery of a kind no parser models. The payload is on the event.
    Unmodelled {
        /// The platform's name for the event.
        name: String,
    },
    /// Parsed, and deliberately nothing to do. The reason is for the record.
    Ignored(String),
}

impl EventKind {
    /// A short name for records and logs.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::PullRequest { .. } => "pull_request",
            Self::Comment { .. } => "comment",
            Self::ReviewRequested { .. } => "review_requested",
            Self::PlanRequested { .. } => "plan_requested",
            Self::AddressRequested { .. } => "address_requested",
            Self::Unmodelled { .. } => "unmodelled",
            Self::Ignored(_) => "ignored",
        }
    }

    /// The repository and number the event is about, when it is about one.
    #[must_use]
    pub fn locator(&self) -> Option<(&RepoRef, u64)> {
        match self {
            Self::PullRequest { repo, number, .. } | Self::Comment { repo, number, .. } => {
                Some((repo, *number))
            }
            Self::ReviewRequested { target, .. } | Self::AddressRequested { target, .. } => {
                Some((&target.repo, target.number))
            }
            Self::PlanRequested { target, .. } => Some((&target.repo, target.number)),
            Self::Unmodelled { .. } | Self::Ignored(_) => None,
        }
    }
}

/// One inbound event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Its id.
    pub id: EventId,
    /// When it was received, RFC 3339.
    pub received_at: String,
    /// Where it came from.
    pub source: EventSource,
    /// What happened.
    pub kind: EventKind,
    /// The raw payload as received. Recorded locally for replay and
    /// diagnosis; it never leaves the service.
    pub payload: Option<Value>,
}

/// What a listener did with an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handled {
    /// Nothing, for this reason.
    Ignored(String),
    /// A run was started.
    Started(RunId),
    /// The event joined a run already going.
    Joined(RunId),
    /// A run was started and an older one cancelled.
    Superseded(RunId),
    /// A greeting was posted.
    Greeted,
    /// The listener failed; the reason is recorded.
    Failed(String),
}

impl Handled {
    /// A short name for records.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ignored(_) => "ignored",
            Self::Started(_) => "started",
            Self::Joined(_) => "joined",
            Self::Superseded(_) => "superseded",
            Self::Greeted => "greeted",
            Self::Failed(_) => "failed",
        }
    }

    /// The run this outcome refers to, if any.
    #[must_use]
    pub fn run(&self) -> Option<&RunId> {
        match self {
            Self::Started(run) | Self::Joined(run) | Self::Superseded(run) => Some(run),
            Self::Ignored(_) | Self::Greeted | Self::Failed(_) => None,
        }
    }

    /// The detail text for records.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Ignored(reason) | Self::Failed(reason) => reason.clone(),
            Self::Started(run) | Self::Joined(run) | Self::Superseded(run) => run.to_string(),
            Self::Greeted => String::new(),
        }
    }
}
