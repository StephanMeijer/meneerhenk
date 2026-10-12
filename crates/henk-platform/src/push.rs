//! What Henk reads on a platform to push to a pull request's branch: the
//! facts that decide whether he may, his commit identity and his git
//! credential; and the one write it makes beside the push, opening the pull
//! request for an issue the executor took on (#329).

use henk_domain::allowlist::RepoRef;
use henk_domain::push::PushFacts;
use henk_domain::review::CommitSha;
use secrecy::SecretString;

use crate::error::PlatformError;
use crate::writer::ReviewTarget;

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

/// A pull or merge request Henk opens for a branch he pushed (#329).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPull {
    /// The branch with the change.
    pub head: String,
    /// The branch it goes into: the default branch.
    pub base: String,
    /// Its title.
    pub title: String,
    /// Its description.
    pub body: String,
}

/// The pull or merge request as opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedPull {
    /// Its number (GitLab: iid).
    pub number: u64,
    /// Where people read it.
    pub url: String,
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

/// What Henk reads on a platform to push, besides git: for the review
/// loop's executor and the planner's copy of the default branch.
#[async_trait::async_trait]
pub trait PushWriter: Send + Sync {
    /// The facts that decide whether Henk may push, and where to.
    async fn pull_facts(&self, target: &ReviewTarget) -> Result<PullFacts, PlatformError>;

    /// The default branch of `repo` and its head commit, and where to fetch
    /// it from.
    async fn repo_head(&self, repo: &RepoRef) -> Result<RepoHead, PlatformError>;

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

    /// Opens a ready pull or merge request of `pull` in `repo` (#329).
    async fn open_pull_request(
        &self,
        repo: &RepoRef,
        pull: &NewPull,
    ) -> Result<OpenedPull, PlatformError>;
}
