//! What pushing to a pull request needs from GitHub beyond the review writer.

use henk_domain::push::PushFacts;
use henk_domain::review::CommitSha;
use serde_json::{Value, json};

use henk_domain::allowlist::RepoRef;

use crate::encode::percent_encode;
use crate::error::PlatformError;
use crate::github::writer::GitHubWriter;
use crate::push::{
    BehindPull, CommitIdentity, GitCredential, NewPull, OpenedPull, PullFacts, PushWriter, RepoHead,
};
use crate::writer::ReviewTarget;

fn text(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// A branch name in a URL path: `/` stays (GitHub routes it), `%` and `#`
/// would end or garble the path.
fn path_segment(branch: &str) -> String {
    branch.replace('%', "%25").replace('#', "%23")
}

#[async_trait::async_trait]
impl PushWriter for GitHubWriter {
    async fn pull_facts(&self, target: &ReviewTarget) -> Result<PullFacts, PlatformError> {
        let pr = self
            .api()
            .get(&format!(
                "/repos/{}/pulls/{}",
                target.repo.path(),
                target.number
            ))
            .await?;
        let head = pr
            .pointer("/head/sha")
            .and_then(Value::as_str)
            .and_then(|sha| CommitSha::parse(sha).ok())
            .ok_or_else(|| PlatformError::Decode("pull request without head sha".to_owned()))?;
        let head_repo = pr
            .pointer("/head/repo/full_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let head_ref = text(&pr, "/head/ref");
        let merged = crate::github::merged(&pr);
        let open = pr.get("state").and_then(Value::as_str) == Some("open") && !merged;
        let head_protected = match &head_repo {
            Some(repo) if !head_ref.is_empty() => self
                .api()
                .get(&format!(
                    "/repos/{repo}/branches/{}",
                    path_segment(&head_ref)
                ))
                .await?
                .get("protected")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            // Unknown counts as protected: refusing is the safe answer.
            _ => true,
        };
        let remote = self
            .api()
            .git_remote(head_repo.as_deref().unwrap_or(&target.repo.path()));
        Ok(PullFacts {
            push: PushFacts {
                open,
                merged,
                head_repo,
                base_repo: target.repo.path(),
                head_ref,
                default_branch: text(&pr, "/base/repo/default_branch"),
                head_protected,
            },
            head,
            remote,
            base_ref: text(&pr, "/base/ref"),
        })
    }

    async fn git_credential(&self) -> Result<Option<GitCredential>, PlatformError> {
        Ok(Some(GitCredential::github(self.api().git_token().await?)))
    }

    async fn repo_head(&self, repo: &RepoRef) -> Result<RepoHead, PlatformError> {
        let path = repo.path();
        let info = self.api().get(&format!("/repos/{path}")).await?;
        let default_branch = text(&info, "/default_branch");
        if default_branch.is_empty() {
            return Err(PlatformError::Decode(
                "repository without a default branch".to_owned(),
            ));
        }
        let branch = self
            .api()
            .get(&format!(
                "/repos/{path}/branches/{}",
                path_segment(&default_branch)
            ))
            .await?;
        let head = branch
            .pointer("/commit/sha")
            .and_then(Value::as_str)
            .and_then(|sha| CommitSha::parse(sha).ok())
            .ok_or_else(|| PlatformError::Decode("branch without a head commit".to_owned()))?;
        Ok(RepoHead {
            default_branch,
            head,
            remote: self.api().git_remote(&path),
        })
    }

    async fn commit_identity(&self) -> Result<CommitIdentity, PlatformError> {
        let login = self.bot_login().to_owned();
        let user = self.api().get(&format!("/users/{login}")).await?;
        let id = user
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::Decode(format!("user {login} without id")))?;
        Ok(CommitIdentity {
            email: format!("{id}+{login}@users.noreply.github.com"),
            name: login,
        })
    }

    fn noreply_host(&self) -> Result<String, PlatformError> {
        Ok("github.com".to_owned())
    }

    async fn open_pull_request(
        &self,
        repo: &RepoRef,
        pull: &NewPull,
    ) -> Result<OpenedPull, PlatformError> {
        let path = format!("/repos/{}/pulls", repo.path());
        let request = json!({
            "title": pull.title,
            "head": pull.head,
            "base": pull.base,
            "body": pull.body,
            "draft": false,
        });
        // GitHub names a head as owner:branch when it looks one up.
        let head = if pull.head.contains(':') {
            pull.head.clone()
        } else {
            format!("{}:{}", repo.owner(), pull.head)
        };
        let opened = crate::retry::create_once(
            self.api().retry_policy(),
            &format!("POST {path}"),
            || self.api().post(&path, &request),
            || async {
                let open = self
                    .api()
                    .get(&format!(
                        "{path}?state=open&head={}&base={}",
                        percent_encode(&head),
                        percent_encode(&pull.base)
                    ))
                    .await?;
                Ok(open.as_array().and_then(|pulls| pulls.first()).cloned())
            },
        )
        .await?;
        let number = opened
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::Decode("pull request without a number".to_owned()))?;
        Ok(OpenedPull {
            number,
            url: text(&opened, "/html_url"),
        })
    }

    async fn open_pulls_behind(
        &self,
        repo: &RepoRef,
        base: &str,
        head_prefix: &str,
    ) -> Result<Vec<BehindPull>, PlatformError> {
        let path = repo.path();
        let pulls = self
            .api()
            .get_all(&format!(
                "/repos/{path}/pulls?state=open&base={}",
                percent_encode(base)
            ))
            .await?;
        let mut behind = Vec::new();
        for pull in pulls {
            let head_ref = text(&pull, "/head/ref");
            let in_repo = pull
                .pointer("/head/repo/full_name")
                .and_then(Value::as_str)
                .is_some_and(|head| head.eq_ignore_ascii_case(&path));
            let draft = pull.get("draft").and_then(Value::as_bool).unwrap_or(false);
            if !in_repo || draft || !head_ref.starts_with(head_prefix) {
                continue;
            }
            let (Some(number), Some(head)) = (
                pull.get("number").and_then(Value::as_u64),
                pull.pointer("/head/sha")
                    .and_then(Value::as_str)
                    .and_then(|sha| CommitSha::parse(sha).ok()),
            ) else {
                continue;
            };
            // `behind_by` is on every comparison, however long its commit
            // list, so one commit of it is all that is asked for.
            let comparison = self
                .api()
                .get(&format!(
                    "/repos/{path}/compare/{}...{}?per_page=1",
                    path_segment(base),
                    head.as_str()
                ))
                .await?;
            let behind_by = comparison
                .get("behind_by")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if behind_by > 0 {
                behind.push(BehindPull {
                    number,
                    head_ref,
                    head,
                    behind_by,
                });
            }
        }
        Ok(behind)
    }

    async fn user_login(&self, id: u64) -> Result<String, PlatformError> {
        let user = self.api().get(&format!("/user/{id}")).await?;
        user.get("login")
            .and_then(Value::as_str)
            .filter(|login| !login.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| PlatformError::Decode(format!("user {id} without login")))
    }
}
