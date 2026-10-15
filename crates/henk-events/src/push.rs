//! A push to a repository's default branch as an event (#579), the same for
//! both platforms: what a pull request on that branch falls behind.

use henk_domain::allowlist::RepoRef;
use henk_domain::review::CommitSha;

use crate::event::{EventKind, Sender};

/// What a push delivery says, platform-neutral: the ref pushed (`refs/heads/main`),
/// the repository's default branch, the commit the branch is at now, and
/// whether the platform says the branch was deleted.
pub(crate) struct Pushed<'a> {
    pub(crate) git_ref: Option<&'a str>,
    pub(crate) default_branch: Option<&'a str>,
    pub(crate) after: Option<&'a str>,
    pub(crate) deleted: bool,
}

/// The event for a push: [`EventKind::BranchPushed`] when it moved the
/// default branch, and otherwise [`EventKind::Ignored`] with the reason. A
/// push to any other branch, a tag or a deletion moves no pull request's
/// base that Henk looks at.
pub(crate) fn pushed(repo: RepoRef, push: &Pushed<'_>, sender: Sender) -> EventKind {
    let Some(branch) = push
        .git_ref
        .and_then(|name| name.strip_prefix("refs/heads/"))
    else {
        return EventKind::Ignored("push: not a branch".to_owned());
    };
    let Some(head) = push.after.and_then(|sha| CommitSha::parse(sha).ok()) else {
        return EventKind::Ignored("push without a head commit".to_owned());
    };
    // An all-zero commit is how a platform says the branch is gone.
    if push.deleted || head.as_str().bytes().all(|b| b == b'0') {
        return EventKind::Ignored(format!("push: {branch} was deleted"));
    }
    if push.default_branch != Some(branch) {
        return EventKind::Ignored(format!("push: {branch} is not the default branch"));
    }
    EventKind::BranchPushed {
        repo,
        branch: branch.to_owned(),
        head,
        sender,
    }
}
