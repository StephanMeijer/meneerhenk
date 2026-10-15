//! What pushing to a merge request needs from GitLab beyond the review writer.
//! The merge request's projects, the branch's protection and the bot's
//! account come from REST, because the MCP server's merge request leaves
//! out the project ids.

use henk_domain::commit::noreply;
use henk_domain::push::PushFacts;
use henk_domain::review::CommitSha;
use serde_json::{Value, json};

use henk_domain::allowlist::RepoRef;

use crate::error::PlatformError;
use crate::gitlab::rest::path_segment;
use crate::gitlab::writer::GitLabWriter;
use crate::gitlab::{number, text};
use crate::push::{
    CommitIdentity, GitCredential, NewPull, OpenedPull, PullFacts, PushWriter, RepoHead,
};
use crate::writer::ReviewTarget;

fn flag(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

#[async_trait::async_trait]
impl PushWriter for GitLabWriter {
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
                merged: mr.get("state").and_then(Value::as_str) == Some("merged"),
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

    async fn open_pull_request(
        &self,
        repo: &RepoRef,
        pull: &NewPull,
    ) -> Result<OpenedPull, PlatformError> {
        // Through the gitlab-write MCP server, like Henk's other GitLab
        // writes; its token needs Developer on the project.
        let opened = self
            .call_tool(
                "create_merge_request",
                json!({
                    "project_id": repo.path(),
                    "source_branch": pull.head,
                    "target_branch": pull.base,
                    "title": pull.title,
                    "description": pull.body,
                }),
            )
            .await?;
        let number = number(opened.get("iid"))
            .ok_or_else(|| PlatformError::Decode("merge request without an iid".to_owned()))?;
        Ok(OpenedPull {
            number,
            url: text(&opened, "web_url"),
        })
    }

    async fn user_login(&self, id: u64) -> Result<String, PlatformError> {
        let user = self.rest()?.get(&format!("/users/{id}")).await?;
        user.get("username")
            .and_then(Value::as_str)
            .filter(|username| !username.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| PlatformError::Decode(format!("user {id} without username")))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use serde_json::json;

    use super::*;

    #[test]
    fn ids_come_as_numbers_or_strings() {
        assert_eq!(number(Some(&json!(7))), Some(7));
        assert_eq!(number(Some(&json!("7"))), Some(7));
        assert_eq!(number(Some(&json!("x"))), None);
        assert_eq!(number(None), None);
    }
}
