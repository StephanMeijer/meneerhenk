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

/// Parses a GitLab webhook delivery. `event` is the `X-Gitlab-Event` header.
#[must_use]
pub fn parse_gitlab(event: &str, payload: &Value) -> IncomingEvent {
    let Some(repo) = payload
        .pointer("/project/path_with_namespace")
        .and_then(Value::as_str)
        .and_then(|path| RepoRef::parse(Platform::GitLab, path).ok())
    else {
        return IncomingEvent::Ignored(format!("{event}: no project"));
    };
    let sender = Sender {
        login: payload
            .pointer("/user/username")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        is_bot: payload
            .pointer("/user/bot")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let kind = payload
        .get("object_kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "merge_request" => parse_gitlab_merge_request(payload, repo, sender),
        "note" => parse_gitlab_note(payload, repo, sender),
        other => IncomingEvent::Ignored(format!("object_kind {other}")),
    }
}

fn parse_gitlab_merge_request(payload: &Value, repo: RepoRef, sender: Sender) -> IncomingEvent {
    let attributes = payload
        .get("object_attributes")
        .cloned()
        .unwrap_or(Value::Null);
    let action = match attributes.get("action").and_then(Value::as_str) {
        Some("open") => PullRequestAction::Opened,
        Some("reopen") => PullRequestAction::Reopened,
        // GitLab sends "update" for pushes and for every other edit; the
        // caller compares the head sha with what it reviewed last.
        Some("update") if attributes.get("oldrev").is_some_and(|v| !v.is_null()) => {
            PullRequestAction::Synchronized
        }
        Some(other) => return IncomingEvent::Ignored(format!("merge_request.{other}")),
        None => return IncomingEvent::Ignored("merge_request without action".to_owned()),
    };
    let Some(number) = attributes.get("iid").and_then(Value::as_u64) else {
        return IncomingEvent::Ignored("merge_request without iid".to_owned());
    };
    let Some(head) = attributes
        .pointer("/last_commit/id")
        .and_then(Value::as_str)
        .and_then(|sha| CommitSha::parse(sha).ok())
    else {
        return IncomingEvent::Ignored("merge_request without last commit".to_owned());
    };
    let draft = attributes
        .get("draft")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || attributes
            .get("work_in_progress")
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

fn parse_gitlab_note(payload: &Value, repo: RepoRef, sender: Sender) -> IncomingEvent {
    let attributes = payload
        .get("object_attributes")
        .cloned()
        .unwrap_or(Value::Null);
    if attributes.get("noteable_type").and_then(Value::as_str) != Some("MergeRequest") {
        return IncomingEvent::Ignored("note on something other than a merge request".to_owned());
    }
    if payload
        .pointer("/merge_request/state")
        .and_then(Value::as_str)
        != Some("opened")
    {
        return IncomingEvent::Ignored("note on a merge request that is not open".to_owned());
    }
    let Some(number) = payload
        .pointer("/merge_request/iid")
        .and_then(Value::as_u64)
    else {
        return IncomingEvent::Ignored("note without merge request iid".to_owned());
    };
    let on_diff = attributes.get("type").and_then(Value::as_str) == Some("DiffNote")
        || attributes.get("position").is_some_and(|v| !v.is_null());
    IncomingEvent::Comment {
        repo,
        number,
        body: attributes
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        comment_id: attributes
            .get("id")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .to_string(),
        kind: if on_diff {
            CommentKind::Review {
                in_reply_to: attributes
                    .get("discussion_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        } else {
            CommentKind::Conversation
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
    fn gitlab_merge_request_push_is_synchronized() {
        let payload = json!({
            "object_kind": "merge_request",
            "project": {"path_with_namespace": "9xxlab/tools/cli"},
            "user": {"username": "alice"},
            "object_attributes": {
                "iid": 5, "action": "update", "oldrev": "abc", "state": "opened",
                "draft": false, "last_commit": {"id": SHA}
            }
        });
        let event = parse_gitlab("Merge Request Hook", &payload);
        assert!(matches!(
            &event,
            IncomingEvent::PullRequest { number: 5, action: PullRequestAction::Synchronized, repo, .. }
                if repo.path() == "9xxlab/tools/cli"
        ));
        let mut edited = payload.clone();
        edited["object_attributes"]["oldrev"] = Value::Null;
        assert!(
            matches!(
                parse_gitlab("Merge Request Hook", &edited),
                IncomingEvent::Ignored(_)
            ),
            "title edit"
        );
        let mut opened = payload;
        opened["object_attributes"]["action"] = json!("open");
        assert!(matches!(
            parse_gitlab("Merge Request Hook", &opened),
            IncomingEvent::PullRequest {
                action: PullRequestAction::Opened,
                ..
            }
        ));
    }

    #[test]
    fn gitlab_notes_on_open_merge_requests_are_comments() {
        let payload = json!({
            "object_kind": "note",
            "project": {"path_with_namespace": "9xxlab/app"},
            "user": {"username": "bob"},
            "merge_request": {"iid": 9, "state": "opened"},
            "object_attributes": {"id": 77, "note": "@meneerhenk review", "noteable_type": "MergeRequest", "type": null}
        });
        let event = parse_gitlab("Note Hook", &payload);
        assert!(matches!(
            &event,
            IncomingEvent::Comment { number: 9, comment_id, kind: CommentKind::Conversation, .. } if comment_id == "77"
        ));
        let mut diff_note = payload.clone();
        diff_note["object_attributes"]["type"] = json!("DiffNote");
        diff_note["object_attributes"]["discussion_id"] = json!("d1");
        assert!(matches!(
            parse_gitlab("Note Hook", &diff_note),
            IncomingEvent::Comment {
                kind: CommentKind::Review {
                    in_reply_to: Some(_)
                },
                ..
            }
        ));
        let mut closed = payload;
        closed["merge_request"]["state"] = json!("merged");
        assert!(matches!(
            parse_gitlab("Note Hook", &closed),
            IncomingEvent::Ignored(_)
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
