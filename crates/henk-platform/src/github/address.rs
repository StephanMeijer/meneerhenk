//! What an address run (§3.5) needs from GitHub beyond the review writer.

use henk_domain::address::PushFacts;
use henk_domain::review::CommitSha;
use serde_json::Value;

use crate::address::{
    AddressWriter, CommitIdentity, GitCredential, OpenThread, PullFacts, ThreadNote,
};
use crate::error::PlatformError;
use crate::github::writer::GitHubWriter;
use crate::writer::{PlatformWriter as _, PostedComment, ReviewTarget};

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
impl AddressWriter for GitHubWriter {
    async fn open_threads(&self, target: &ReviewTarget) -> Result<Vec<OpenThread>, PlatformError> {
        let threads = self
            .api()
            .review_threads(target.repo.owner(), target.repo.name(), target.number)
            .await?;
        Ok(threads
            .into_iter()
            .filter(|t| !t.resolved && !t.id.is_empty() && !t.comments.is_empty())
            .map(|t| OpenThread {
                thread_id: t.id,
                path: t.path,
                line: t.line,
                outdated: t.outdated,
                notes: t
                    .comments
                    .into_iter()
                    .map(|c| ThreadNote {
                        comment_id: c.database_id.to_string(),
                        by_henk: c.author == self.bot_login(),
                        author: c.author,
                        author_id: c.author_id,
                        body: c.body,
                    })
                    .collect(),
            })
            .collect())
    }

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
        let open = pr.get("state").and_then(Value::as_str) == Some("open")
            && !pr.get("merged").and_then(Value::as_bool).unwrap_or(false);
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
                head_repo,
                base_repo: target.repo.path(),
                head_ref,
                default_branch: text(&pr, "/base/repo/default_branch"),
                head_protected,
            },
            head,
            remote,
        })
    }

    async fn git_credential(&self) -> Result<Option<GitCredential>, PlatformError> {
        Ok(Some(GitCredential::github(self.api().git_token().await?)))
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

    async fn user_login(&self, id: u64) -> Result<String, PlatformError> {
        let user = self.api().get(&format!("/user/{id}")).await?;
        user.get("login")
            .and_then(Value::as_str)
            .filter(|login| !login.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| PlatformError::Decode(format!("user {id} without login")))
    }

    async fn resolve_thread(
        &self,
        _target: &ReviewTarget,
        thread_id: &str,
    ) -> Result<(), PlatformError> {
        self.api().resolve_review_thread(thread_id).await
    }

    fn commit_url(&self, target: &ReviewTarget, sha: &str) -> String {
        let remote = self.api().git_remote(&target.repo.path());
        format!("{}/commit/{sha}", remote.trim_end_matches(".git"))
    }

    async fn reply_in_thread(
        &self,
        target: &ReviewTarget,
        thread: &OpenThread,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        let first = thread
            .first()
            .ok_or_else(|| PlatformError::Decode("thread without comments".to_owned()))?;
        self.reply(target, &first.comment_id, true, body).await
    }

    async fn post_comment(
        &self,
        target: &ReviewTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError> {
        crate::writer::PlatformWriter::post_comment(self, target, body).await
    }
}
