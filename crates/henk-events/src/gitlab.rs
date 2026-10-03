//! GitLab webhook payloads as events.

use henk_domain::allowlist::{Platform, RepoRef};
use henk_domain::review::CommitSha;
use serde_json::Value;

use crate::event::{CommentKind, EventKind, PullRequestAction, Sender};

/// Parses a GitLab webhook delivery. `event` is the `X-Gitlab-Event` header.
#[must_use]
pub fn parse_gitlab(event: &str, payload: &Value) -> EventKind {
    let Some(repo) = payload
        .pointer("/project/path_with_namespace")
        .and_then(Value::as_str)
        .and_then(|path| RepoRef::parse(Platform::GitLab, path).ok())
    else {
        return EventKind::Ignored(format!("{event}: no project"));
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
        other => EventKind::Unmodelled {
            name: other.to_owned(),
        },
    }
}

fn parse_gitlab_merge_request(payload: &Value, repo: RepoRef, sender: Sender) -> EventKind {
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
        Some(other) => return EventKind::Ignored(format!("merge_request.{other}")),
        None => return EventKind::Ignored("merge_request without action".to_owned()),
    };
    let Some(number) = attributes.get("iid").and_then(Value::as_u64) else {
        return EventKind::Ignored("merge_request without iid".to_owned());
    };
    let Some(head) = attributes
        .pointer("/last_commit/id")
        .and_then(Value::as_str)
        .and_then(|sha| CommitSha::parse(sha).ok())
    else {
        return EventKind::Ignored("merge_request without last commit".to_owned());
    };
    let draft = attributes
        .get("draft")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || attributes
            .get("work_in_progress")
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

fn parse_gitlab_note(payload: &Value, repo: RepoRef, sender: Sender) -> EventKind {
    let attributes = payload
        .get("object_attributes")
        .cloned()
        .unwrap_or(Value::Null);
    if attributes.get("noteable_type").and_then(Value::as_str) != Some("MergeRequest") {
        return EventKind::Ignored("note on something other than a merge request".to_owned());
    }
    if payload
        .pointer("/merge_request/state")
        .and_then(Value::as_str)
        != Some("opened")
    {
        return EventKind::Ignored("note on a merge request that is not open".to_owned());
    }
    let Some(number) = payload
        .pointer("/merge_request/iid")
        .and_then(Value::as_u64)
    else {
        return EventKind::Ignored("note without merge request iid".to_owned());
    };
    let on_diff = attributes.get("type").and_then(Value::as_str) == Some("DiffNote")
        || attributes.get("position").is_some_and(|v| !v.is_null());
    EventKind::Comment {
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
