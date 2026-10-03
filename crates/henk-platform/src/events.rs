//! Webhook payloads as platform-neutral events.

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::review::CommitSha;
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

/// An event Henk may act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncomingEvent {
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
    /// Something Henk does not act on. The reason is for logs.
    Ignored(String),
}

/// Parses a GitHub webhook delivery. `event` is the `X-GitHub-Event` header.
#[must_use]
pub fn parse_github(event: &str, payload: &Value) -> IncomingEvent {
    let Some(repo) = payload
        .pointer("/repository/full_name")
        .and_then(Value::as_str)
        .and_then(|full| RepoRef::parse(Platform::GitHub, full).ok())
    else {
        return IncomingEvent::Ignored(format!("{event}: no repository"));
    };
    let sender = Sender {
        login: payload
            .pointer("/sender/login")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        is_bot: payload.pointer("/sender/type").and_then(Value::as_str) == Some("Bot"),
    };
    let action = payload.get("action").and_then(Value::as_str).unwrap_or("");

    match event {
        "pull_request" => parse_pull_request(action, payload, repo, sender),
        "issue_comment" => parse_issue_comment(action, payload, repo, sender),
        "pull_request_review_comment" => parse_review_comment(action, payload, repo, sender),
        other => IncomingEvent::Ignored(format!("event {other}")),
    }
}

fn parse_pull_request(
    action: &str,
    payload: &Value,
    repo: RepoRef,
    sender: Sender,
) -> IncomingEvent {
    let action = match action {
        "opened" | "ready_for_review" => PullRequestAction::Opened,
        "reopened" => PullRequestAction::Reopened,
        "synchronize" => PullRequestAction::Synchronized,
        other => return IncomingEvent::Ignored(format!("pull_request.{other}")),
    };
    let Some(number) = payload
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
    else {
        return IncomingEvent::Ignored("pull_request without number".to_owned());
    };
    let Some(head) = payload
        .pointer("/pull_request/head/sha")
        .and_then(Value::as_str)
        .and_then(|sha| CommitSha::parse(sha).ok())
    else {
        return IncomingEvent::Ignored("pull_request without head sha".to_owned());
    };
    let draft = payload
        .pointer("/pull_request/draft")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    IncomingEvent::PullRequest {
        repo,
        number,
        action,
        head,
        draft,
        sender,
    }
}

fn parse_issue_comment(
    action: &str,
    payload: &Value,
    repo: RepoRef,
    sender: Sender,
) -> IncomingEvent {
    if action != "created" {
        return IncomingEvent::Ignored(format!("issue_comment.{action}"));
    }
    if payload.pointer("/issue/pull_request").is_none() {
        return IncomingEvent::Ignored("comment on an issue, not a pull request".to_owned());
    }
    if payload.pointer("/issue/state").and_then(Value::as_str) != Some("open") {
        return IncomingEvent::Ignored("comment on a closed pull request".to_owned());
    }
    let Some(number) = payload.pointer("/issue/number").and_then(Value::as_u64) else {
        return IncomingEvent::Ignored("issue_comment without number".to_owned());
    };
    IncomingEvent::Comment {
        repo,
        number,
        body: comment_body(payload),
        comment_id: comment_id(payload),
        kind: CommentKind::Conversation,
        sender,
    }
}

fn parse_review_comment(
    action: &str,
    payload: &Value,
    repo: RepoRef,
    sender: Sender,
) -> IncomingEvent {
    if action != "created" {
        return IncomingEvent::Ignored(format!("pull_request_review_comment.{action}"));
    }
    if payload
        .pointer("/pull_request/state")
        .and_then(Value::as_str)
        != Some("open")
    {
        return IncomingEvent::Ignored("review comment on a closed pull request".to_owned());
    }
    let Some(number) = payload
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
    else {
        return IncomingEvent::Ignored("review comment without number".to_owned());
    };
    IncomingEvent::Comment {
        repo,
        number,
        body: comment_body(payload),
        comment_id: comment_id(payload),
        kind: CommentKind::Review {
            in_reply_to: payload
                .pointer("/comment/in_reply_to_id")
                .and_then(Value::as_u64)
                .map(|id| id.to_string()),
        },
        sender,
    }
}

fn comment_body(payload: &Value) -> String {
    payload
        .pointer("/comment/body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn comment_id(payload: &Value) -> String {
    payload
        .pointer("/comment/id")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use serde_json::json;

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn base() -> Value {
        json!({
            "repository": {"full_name": "docspec/app"},
            "sender": {"login": "alice", "type": "User"},
        })
    }

    #[test]
    fn pull_request_synchronize_is_parsed() {
        let mut payload = base();
        payload["action"] = json!("synchronize");
        payload["pull_request"] = json!({"number": 7, "head": {"sha": SHA}, "draft": false});
        let event = parse_github("pull_request", &payload);
        assert!(matches!(
            event,
            IncomingEvent::PullRequest {
                number: 7,
                action: PullRequestAction::Synchronized,
                draft: false,
                ..
            }
        ));
    }

    #[test]
    fn ready_for_review_counts_as_opened_and_closed_is_ignored() {
        let mut payload = base();
        payload["action"] = json!("ready_for_review");
        payload["pull_request"] = json!({"number": 7, "head": {"sha": SHA}, "draft": false});
        assert!(matches!(
            parse_github("pull_request", &payload),
            IncomingEvent::PullRequest {
                action: PullRequestAction::Opened,
                ..
            }
        ));
        payload["action"] = json!("closed");
        assert!(matches!(
            parse_github("pull_request", &payload),
            IncomingEvent::Ignored(_)
        ));
    }

    #[test]
    fn issue_comment_on_a_pull_request_is_a_comment() {
        let mut payload = base();
        payload["action"] = json!("created");
        payload["issue"] = json!({"number": 7, "state": "open", "pull_request": {"url": "x"}});
        payload["comment"] = json!({"id": 99, "body": "@meneer-henk review"});
        let event = parse_github("issue_comment", &payload);
        assert!(matches!(
            &event,
            IncomingEvent::Comment { number: 7, body, comment_id, kind: CommentKind::Conversation, .. }
                if body == "@meneer-henk review" && comment_id == "99"
        ));
        payload["issue"] = json!({"number": 7, "state": "open"});
        assert!(
            matches!(
                parse_github("issue_comment", &payload),
                IncomingEvent::Ignored(_)
            ),
            "plain issue"
        );
    }

    #[test]
    fn review_comment_replies_carry_their_parent() {
        let mut payload = base();
        payload["action"] = json!("created");
        payload["pull_request"] = json!({"number": 7, "state": "open"});
        payload["comment"] = json!({"id": 5, "body": "thanks @meneer-henk", "in_reply_to_id": 4});
        payload["sender"] = json!({"login": "meneer-henk[bot]", "type": "Bot"});
        let event = parse_github("pull_request_review_comment", &payload);
        assert!(matches!(
            &event,
            IncomingEvent::Comment { kind: CommentKind::Review { in_reply_to: Some(parent) }, sender, .. }
                if parent == "4" && sender.is_bot
        ));
    }

    #[test]
    fn unknown_events_are_ignored() {
        assert!(matches!(
            parse_github("push", &base()),
            IncomingEvent::Ignored(_)
        ));
        assert!(matches!(
            parse_github("pull_request", &json!({})),
            IncomingEvent::Ignored(_)
        ));
    }
}
