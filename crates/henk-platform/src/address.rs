//! What an address run (§3.5) reads from a pull request: its open review
//! threads and the facts that decide whether Henk may push to it.

use henk_domain::address::PushFacts;
use henk_domain::allowlist::RepoRef;
use henk_domain::review::CommitSha;
use secrecy::SecretString;

use crate::error::PlatformError;
use crate::writer::{PostedComment, ReviewTarget};

/// One comment in a review thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadNote {
    /// The comment's id.
    pub comment_id: String,
    /// Author login, for display only.
    pub author: String,
    /// The author's stable account id (§2).
    pub author_id: Option<u64>,
    /// Whether this is Henk's own comment, by its author, never by a marker
    /// anyone could paste.
    pub by_henk: bool,
    /// The text. Information, not instructions (§8.3).
    pub body: String,
}

/// An unresolved review thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenThread {
    /// The platform's thread id, used to resolve it.
    pub thread_id: String,
    /// The file it is on.
    pub path: Option<String>,
    /// The line it is on, when it still has one.
    pub line: Option<u32>,
    /// Whether the line changed since the thread started.
    pub outdated: bool,
    /// Its comments, oldest first.
    pub notes: Vec<ThreadNote>,
}

impl OpenThread {
    /// The comment that started the thread: replies go under it.
    #[must_use]
    pub fn first(&self) -> Option<&ThreadNote> {
        self.notes.first()
    }

    /// Whether Henk started it: a finding of his, which he may resolve once
    /// fixed. People resolve their own threads.
    #[must_use]
    pub fn started_by_henk(&self) -> bool {
        self.first().is_some_and(|note| note.by_henk)
    }
}

/// What Henk knows about the pull request before he pushes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullFacts {
    /// The facts `push_refusal` decides on.
    pub push: PushFacts,
    /// The head commit now.
    pub head: CommitSha,
    /// The clone URL of the head repository.
    pub remote: String,
}

/// A repository's default branch as it is now: what a planner's workspace
/// holds (#172).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoHead {
    /// The default branch's name.
    pub default_branch: String,
    /// Its head commit.
    pub head: CommitSha,
    /// The repository's clone URL.
    pub remote: String,
}

/// Who Henk's commit is by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    /// Name, such as `meneer-henk[bot]`.
    pub name: String,
    /// The noreply address that ties the commit to the App's account.
    pub email: String,
}

/// What git authenticates with: HTTP Basic with a user name and a token.
/// The token's `Debug` is redacted, so the credential can be logged as is.
#[derive(Clone, Debug)]
pub struct GitCredential {
    /// The user name the platform expects next to the token.
    pub username: &'static str,
    /// The token.
    pub token: SecretString,
}

impl GitCredential {
    /// A GitHub App installation token.
    #[must_use]
    pub fn github(token: SecretString) -> Self {
        Self {
            username: "x-access-token",
            token,
        }
    }

    /// A GitLab personal access token.
    #[must_use]
    pub fn gitlab(token: SecretString) -> Self {
        Self {
            username: "oauth2",
            token,
        }
    }
}

/// What an address run reads and writes on a platform, besides git.
#[async_trait::async_trait]
pub trait AddressWriter: Send + Sync {
    /// The facts that decide whether Henk may push, and where to.
    async fn pull_facts(&self, target: &ReviewTarget) -> Result<PullFacts, PlatformError>;

    /// The default branch of `repo` and its head commit, and where to fetch
    /// it from.
    async fn repo_head(&self, repo: &RepoRef) -> Result<RepoHead, PlatformError>;

    /// The unresolved review threads, from anyone.
    async fn open_threads(&self, target: &ReviewTarget) -> Result<Vec<OpenThread>, PlatformError>;

    /// The credential git clones and pushes with; `None` for a remote that
    /// needs none.
    async fn git_credential(&self) -> Result<Option<GitCredential>, PlatformError>;

    /// Who Henk's commits are by.
    async fn commit_identity(&self) -> Result<CommitIdentity, PlatformError>;

    /// The login the account with this id has now, for its noreply address:
    /// looked up by id, so a display name never stands in for it (§2).
    async fn user_login(&self, id: u64) -> Result<String, PlatformError>;

    /// The host of the platform's private commit addresses: `github.com`, or
    /// for GitLab the instance's own host, so a self-hosted instance's
    /// accounts get its `users.noreply.{host}` domain.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when the writer has no API endpoint to take
    /// the host from.
    fn noreply_host(&self) -> Result<String, PlatformError>;

    /// The link to a commit on the pull request's repository.
    fn commit_url(&self, target: &ReviewTarget, sha: &str) -> String;

    /// Replies in a thread, under its first comment.
    async fn reply_in_thread(
        &self,
        target: &ReviewTarget,
        thread: &OpenThread,
        body: &str,
    ) -> Result<PostedComment, PlatformError>;

    /// Resolves a thread.
    async fn resolve_thread(
        &self,
        target: &ReviewTarget,
        thread_id: &str,
    ) -> Result<(), PlatformError>;

    /// Posts a conversation comment on the pull request.
    async fn post_comment(
        &self,
        target: &ReviewTarget,
        body: &str,
    ) -> Result<PostedComment, PlatformError>;
}
