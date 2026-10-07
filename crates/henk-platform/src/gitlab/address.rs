//! What an address run (§3.5) needs from GitLab beyond the review writer.
//! The merge request's projects, the branch's protection and the bot's
//! account come from REST, because the MCP server's merge request leaves
//! out the project ids; discussions, replies and resolving go through the
//! write-mode MCP session as review findings do.

use henk_domain::address::PushFacts;
use henk_domain::commit::noreply;
use henk_domain::review::CommitSha;
use serde_json::{Value, json};

use henk_domain::allowlist::RepoRef;

use crate::address::{
    AddressWriter, CommitIdentity, GitCredential, OpenThread, PullFacts, RepoHead, ThreadNote,
};
use crate::error::PlatformError;
use crate::gitlab::rest::path_segment;
use crate::gitlab::writer::{GitLabWriter, is_system, note_body, note_id};
use crate::writer::{PlatformWriter as _, PostedComment, ReviewTarget};

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// An id GitLab or the MCP server gives as a number or as a string.
fn number(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn flag(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

/// One diff discussion as an open thread, or `None` when it is resolved,
/// not on the diff, or not a thread anyone can resolve.
fn open_thread(discussion: &Value, head: Option<&str>, bot_id: u64) -> Option<OpenThread> {
    let notes = discussion.get("notes").and_then(Value::as_array)?;
    let first = notes.first()?;
    let position = first.get("position").filter(|p| !p.is_null())?;
    if is_system(first) || flag(first, "resolvable") == Some(false) {
        return None;
    }
    if flag(discussion, "resolved").or_else(|| flag(first, "resolved")) == Some(true) {
        return None;
    }
    let thread_id = discussion.get("id").and_then(Value::as_str)?.to_owned();
    let path = position
        .get("new_path")
        .and_then(Value::as_str)
        .or_else(|| position.get("old_path").and_then(Value::as_str))
        .map(str::to_owned);
    let line = number(position.get("new_line"))
        .or_else(|| number(position.get("old_line")))
        .and_then(|l| u32::try_from(l).ok());
    let outdated = match (position.get("head_sha").and_then(Value::as_str), head) {
        (Some(at), Some(head)) => !at.eq_ignore_ascii_case(head),
        _ => false,
    };
    let notes = notes
        .iter()
        .filter(|n| !is_system(n))
        .filter_map(|note| {
            let author_id = number(note.pointer("/author/id"));
            Some(ThreadNote {
                comment_id: note_id(note)?,
                author: note
                    .pointer("/author/username")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                author_id,
                // By the author's id only: a marker anyone could paste.
                by_henk: author_id == Some(bot_id),
                body: note_body(note).to_owned(),
            })
        })
        .collect::<Vec<_>>();
    (!notes.is_empty()).then_some(OpenThread {
        thread_id,
        path,
        line,
        outdated,
        notes,
    })
}

#[async_trait::async_trait]
impl AddressWriter for GitLabWriter {
    async fn open_threads(&self, target: &ReviewTarget) -> Result<Vec<OpenThread>, PlatformError> {
        let bot_id = self.bot_user().await?.id;
        let merge_request = self.merge_request(target).await?;
        let head = merge_request
            .get("sha")
            .and_then(Value::as_str)
            .or_else(|| {
                merge_request
                    .pointer("/diff_refs/head_sha")
                    .and_then(Value::as_str)
            });
        Ok(self
            .discussions(target)
            .await?
            .iter()
            .filter_map(|d| open_thread(d, head, bot_id))
            .collect())
    }

    async fn pull_facts(&self, target: &ReviewTarget) -> Result<PullFacts, PlatformError> {
        let rest = self.rest()?;
        let project = path_segment(&target.repo.path());
        let mr = rest
            .get(&format!(
                "/projects/{project}/merge_requests/{}",
                target.number
            ))
            .await?;
        let head = mr
            .get("sha")
            .and_then(Value::as_str)
            .and_then(|sha| CommitSha::parse(sha).ok())
            .ok_or_else(|| PlatformError::Decode("merge request without head sha".to_owned()))?;
        let head_ref = text(&mr, "source_branch");
        let base = rest.get(&format!("/projects/{project}")).await?;
        let base_id = number(base.get("id"))
            .ok_or_else(|| PlatformError::Decode("project without id".to_owned()))?;
        let mut default_branch = text(&base, "default_branch");
        // A fork is another project; one that is gone or out of the
        // token's reach is a branch Henk cannot push to.
        let source = match number(mr.get("source_project_id")) {
            Some(id) if id == base_id => Some((id, base.clone())),
            Some(id) => rest
                .get_optional(&format!("/projects/{id}"))
                .await?
                .map(|p| (id, p)),
            None => None,
        };
        let head_repo = source.as_ref().map(|(id, project)| {
            if *id == base_id {
                target.repo.path()
            } else {
                project
                    .get("path_with_namespace")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("project {id}"), str::to_owned)
            }
        });
        let head_protected = match &source {
            Some((id, _)) if !head_ref.is_empty() => {
                match rest
                    .get_optional(&format!(
                        "/projects/{id}/repository/branches/{}",
                        path_segment(&head_ref)
                    ))
                    .await?
                {
                    Some(branch) => {
                        if flag(&branch, "default") == Some(true) {
                            default_branch.clone_from(&head_ref);
                        }
                        flag(&branch, "protected").unwrap_or(true)
                    }
                    // Unknown counts as protected: refusing is the safe answer.
                    None => true,
                }
            }
            _ => true,
        };
        let remote = source
            .as_ref()
            .map(|(_, project)| text(project, "http_url_to_repo"))
            .unwrap_or_default();
        if head_repo.is_some() && remote.is_empty() {
            return Err(PlatformError::Decode(
                "source project without http_url_to_repo".to_owned(),
            ));
        }
        Ok(PullFacts {
            push: PushFacts {
                open: mr.get("state").and_then(Value::as_str) == Some("opened"),
                head_repo,
                base_repo: target.repo.path(),
                head_ref,
                default_branch,
                head_protected,
            },
            head,
            remote,
        })
    }

    async fn git_credential(&self) -> Result<Option<GitCredential>, PlatformError> {
        Ok(Some(self.rest()?.git_credential()))
    }

    async fn repo_head(&self, repo: &RepoRef) -> Result<RepoHead, PlatformError> {
        let rest = self.rest()?;
        let project = path_segment(&repo.path());
        let info = rest.get(&format!("/projects/{project}")).await?;
        let default_branch = text(&info, "default_branch");
        let remote = text(&info, "http_url_to_repo");
        if default_branch.is_empty() || remote.is_empty() {
            return Err(PlatformError::Decode(
                "project without a default branch or http_url_to_repo".to_owned(),
            ));
        }
        let branch = rest
            .get(&format!(
                "/projects/{project}/repository/branches/{}",
                path_segment(&default_branch)
            ))
            .await?;
        let head = branch
            .pointer("/commit/id")
            .and_then(Value::as_str)
            .and_then(|sha| CommitSha::parse(sha).ok())
            .ok_or_else(|| PlatformError::Decode("branch without a head commit".to_owned()))?;
        Ok(RepoHead {
            default_branch,
            head,
            remote,
        })
    }

    async fn commit_identity(&self) -> Result<CommitIdentity, PlatformError> {
        let host = self.noreply_host()?;
        let bot = self.bot_user().await?;
        // GitLab's private commit email ties the commit to the account; a
        // requester's is built by the same rule.
        let email = noreply::gitlab(bot.id, &bot.username, &host)
            .map_err(|e| PlatformError::Decode(e.to_string()))?;
        Ok(CommitIdentity {
            name: bot.username.clone(),
            email: email.to_string(),
        })
    }

    fn noreply_host(&self) -> Result<String, PlatformError> {
        Ok(self.rest()?.host())
    }

    async fn user_login(&self, id: u64) -> Result<String, PlatformError> {
        let user = self.rest()?.get(&format!("/users/{id}")).await?;
        user.get("username")
            .and_then(Value::as_str)
            .filter(|username| !username.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| PlatformError::Decode(format!("user {id} without username")))
    }

    fn commit_url(&self, target: &ReviewTarget, sha: &str) -> String {
        match self.rest() {
            Ok(rest) => format!("{}/{}/-/commit/{sha}", rest.web_url(), target.repo.path()),
            Err(_) => sha.to_owned(),
        }
    }

    async fn reply_in_thread(
        &self,
        target: &ReviewTarget,
        thread: &OpenThread,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        // On GitLab a reply goes to the discussion, not under a note.
        self.reply(target, &thread.thread_id, true, body).await
    }

    async fn resolve_thread(
        &self,
        target: &ReviewTarget,
        thread_id: &str,
    ) -> Result<(), PlatformError> {
        let mut args = Self::base_args(target);
        args.insert("discussion_id".into(), json!(thread_id));
        args.insert("resolved".into(), json!(true));
        self.call_tool("resolve_merge_request_thread", Value::Object(args))
            .await?;
        Ok(())
    }

    async fn post_comment(
        &self,
        target: &ReviewTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        crate::writer::PlatformWriter::post_comment(self, target, body).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn ids_come_as_numbers_or_strings() {
        assert_eq!(number(Some(&json!(7))), Some(7));
        assert_eq!(number(Some(&json!("7"))), Some(7));
        assert_eq!(number(Some(&json!("x"))), None);
        assert_eq!(number(None), None);
    }

    #[test]
    fn an_outdated_thread_is_on_an_older_head() {
        let discussion = json!({"id": "d1", "notes": [{
            "id": "1", "body": "x", "author": {"id": "5", "username": "alice"},
            "position": {"new_path": "a.rs", "new_line": 3, "head_sha": "aaa"}
        }]});
        let thread = open_thread(&discussion, Some("bbb"), 9).unwrap();
        assert!(thread.outdated);
        assert_eq!(thread.notes[0].author_id, Some(5));
        assert!(!open_thread(&discussion, Some("AAA"), 9).unwrap().outdated);
    }
}
