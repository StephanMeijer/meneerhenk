//! GitHub webhook payloads as events.

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::review::CommitSha;
use serde_json::Value;

use crate::event::{CommentKind, EventKind, PullRequestAction, Sender};

/// Parses a GitHub webhook delivery. `event` is the `X-GitHub-Event` header.
#[must_use]
pub fn parse_github(event: &str, payload: &Value) -> EventKind {
    let Some(repo) = payload
        .pointer("/repository/full_name")
        .and_then(Value::as_str)
        .and_then(|full| RepoRef::parse(Platform::GitHub, full).ok())
    else {
        return EventKind::Ignored(format!("{event}: no repository"));
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
        other => EventKind::Unmodelled {
            name: other.to_owned(),
        },
    }
}

fn parse_pull_request(action: &str, payload: &Value, repo: RepoRef, sender: Sender) -> EventKind {
    let action = match action {
        "opened" | "ready_for_review" => PullRequestAction::Opened,
        "reopened" => PullRequestAction::Reopened,
        "synchronize" => PullRequestAction::Synchronized,
        other => return EventKind::Ignored(format!("pull_request.{other}")),
    };
    let Some(number) = payload
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
    else {
        return EventKind::Ignored("pull_request without number".to_owned());
    };
    let Some(head) = payload
        .pointer("/pull_request/head/sha")
        .and_then(Value::as_str)
        .and_then(|sha| CommitSha::parse(sha).ok())
    else {
        return EventKind::Ignored("pull_request without head sha".to_owned());
    };
    let draft = payload
        .pointer("/pull_request/draft")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    EventKind::PullRequest {
        repo,
        number,
        action,
        head,
        draft,
        sender,
    }
}

fn parse_issue_comment(action: &str, payload: &Value, repo: RepoRef, sender: Sender) -> EventKind {
    if action != "created" {
        return EventKind::Ignored(format!("issue_comment.{action}"));
    }
    if payload.pointer("/issue/pull_request").is_none() {
        return EventKind::Ignored("comment on an issue, not a pull request".to_owned());
    }
    if payload.pointer("/issue/state").and_then(Value::as_str) != Some("open") {
        return EventKind::Ignored("comment on a closed pull request".to_owned());
    }
    let Some(number) = payload.pointer("/issue/number").and_then(Value::as_u64) else {
        return EventKind::Ignored("issue_comment without number".to_owned());
    };
    EventKind::Comment {
        repo,
        number,
        body: comment_body(payload),
        comment_id: comment_id(payload),
        kind: CommentKind::Conversation,
        sender,
    }
}

fn parse_review_comment(action: &str, payload: &Value, repo: RepoRef, sender: Sender) -> EventKind {
    if action != "created" {
        return EventKind::Ignored(format!("pull_request_review_comment.{action}"));
    }
    if payload
        .pointer("/pull_request/state")
        .and_then(Value::as_str)
        != Some("open")
    {
        return EventKind::Ignored("review comment on a closed pull request".to_owned());
    }
    let Some(number) = payload
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
    else {
        return EventKind::Ignored("review comment without number".to_owned());
    };
    EventKind::Comment {
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

    use serde_json::{Value, json};

    use crate::event::*;
    use crate::github::parse_github;
    use crate::gitlab::parse_gitlab;

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
            EventKind::PullRequest {
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
            EventKind::PullRequest {
                action: PullRequestAction::Opened,
                ..
            }
        ));
        payload["action"] = json!("closed");
        assert!(matches!(
            parse_github("pull_request", &payload),
            EventKind::Ignored(_)
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
            EventKind::Comment { number: 7, body, comment_id, kind: CommentKind::Conversation, .. }
                if body == "@meneer-henk review" && comment_id == "99"
        ));
        payload["issue"] = json!({"number": 7, "state": "open"});
        assert!(
            matches!(
                parse_github("issue_comment", &payload),
                EventKind::Ignored(_)
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
            EventKind::Comment { kind: CommentKind::Review { in_reply_to: Some(parent) }, sender, .. }
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
            EventKind::PullRequest { number: 5, action: PullRequestAction::Synchronized, repo, .. }
                if repo.path() == "9xxlab/tools/cli"
        ));
        let mut edited = payload.clone();
        edited["object_attributes"]["oldrev"] = Value::Null;
        assert!(
            matches!(
                parse_gitlab("Merge Request Hook", &edited),
                EventKind::Ignored(_)
            ),
            "title edit"
        );
        let mut opened = payload;
        opened["object_attributes"]["action"] = json!("open");
        assert!(matches!(
            parse_gitlab("Merge Request Hook", &opened),
            EventKind::PullRequest {
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
            EventKind::Comment { number: 9, comment_id, kind: CommentKind::Conversation, .. } if comment_id == "77"
        ));
        let mut diff_note = payload.clone();
        diff_note["object_attributes"]["type"] = json!("DiffNote");
        diff_note["object_attributes"]["discussion_id"] = json!("d1");
        assert!(matches!(
            parse_gitlab("Note Hook", &diff_note),
            EventKind::Comment {
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
            EventKind::Ignored(_)
        ));
    }

    #[test]
    fn unknown_events_are_unmodelled_and_broken_ones_ignored() {
        assert!(
            matches!(parse_github("push", &base()), EventKind::Unmodelled { ref name } if name == "push")
        );
        assert!(matches!(
            parse_github("pull_request", &json!({})),
            EventKind::Ignored(_)
        ));
    }
}
