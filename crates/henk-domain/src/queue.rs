//! One run per pull/merge request at a time (open question 2, decided;
//! #580). A review request for the commit already under review joins it;
//! one for a newer commit supersedes it. A rebase changes the commits
//! everything else looks at, so it supersedes a review of the pull
//! request, and a review asked for meanwhile waits until the rebase ends.
//! A second rebase request joins the first.
//!
//! A fix of failing checks (#583) pushes a commit, so it supersedes a
//! review too, and a review asked for meanwhile waits for it. A rebase
//! supersedes a fix, since the head it works on is about to change; a fix
//! asked for while a rebase or another fix runs joins that run, since the
//! checks it would read belong to a head that is being replaced.

use crate::review::CommitSha;

/// What a run does to a pull request, as the queue tells runs apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// A review of this commit.
    Review(CommitSha),
    /// A rebase onto the base branch (#579).
    Rebase,
    /// A fix of the failing checks of the head (#583).
    FixCi,
}

impl Work {
    /// Where it waits in the queue for a slot.
    #[must_use]
    pub const fn priority(&self) -> Priority {
        match self {
            Self::Review(_) => Priority::Review,
            Self::FixCi => Priority::Fix,
            Self::Rebase => Priority::Rebase,
        }
    }

    /// The commit it reviews, for a review.
    #[must_use]
    pub const fn commit(&self) -> Option<&CommitSha> {
        match self {
            Self::Review(commit) => Some(commit),
            Self::Rebase | Self::FixCi => None,
        }
    }
}

/// The order runs take free slots in: a higher priority first, then the
/// order they asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// A review.
    Review,
    /// A fix of failing checks: ahead of reviews, since it changes the
    /// head they would look at.
    Fix,
    /// A rebase: ahead of everything else.
    Rebase,
}

impl Priority {
    /// The word for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Fix => "fix",
            Self::Rebase => "rebase",
        }
    }
}

/// What to do with a new request for a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is running; start.
    Start,
    /// The same work is running; join that run.
    Join,
    /// Other work is running; cancel it and start once it has ended.
    Supersede,
    /// A rebase or a fix is running; start once it has ended, unless it
    /// pushed a new head, whose review then takes this one's place.
    After,
}

/// Decides, given the work running on the pull request (if any) and the
/// work requested now.
#[must_use]
pub fn decide(running: Option<&Work>, requested: &Work) -> Decision {
    match (running, requested) {
        (None, _) => Decision::Start,
        (Some(Work::Rebase), Work::Rebase | Work::FixCi) | (Some(Work::FixCi), Work::FixCi) => {
            Decision::Join
        }
        (Some(Work::Rebase | Work::FixCi), Work::Review(_)) => Decision::After,
        // The fix works on a head the rebase is about to replace.
        (Some(Work::FixCi), Work::Rebase) => Decision::Supersede,
        (Some(Work::Review(active)), Work::Review(asked)) if active == asked => Decision::Join,
        // A newer commit, or a rebase or a fix that replaces the commit.
        (Some(Work::Review(_)), _) => Decision::Supersede,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn one_run_per_pull_request_and_a_rebase_goes_first() {
        let a = Work::Review(CommitSha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap());
        let b = Work::Review(CommitSha::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap());
        let rebase = Work::Rebase;
        let fix = Work::FixCi;
        for (running, requested, decision) in [
            (None, &a, Decision::Start),
            (None, &rebase, Decision::Start),
            (None, &fix, Decision::Start),
            (Some(&a), &a, Decision::Join),
            (Some(&a), &b, Decision::Supersede),
            (Some(&a), &rebase, Decision::Supersede),
            (Some(&a), &fix, Decision::Supersede),
            (Some(&rebase), &a, Decision::After),
            (Some(&rebase), &rebase, Decision::Join),
            (Some(&rebase), &fix, Decision::Join),
            (Some(&fix), &a, Decision::After),
            (Some(&fix), &fix, Decision::Join),
            (Some(&fix), &rebase, Decision::Supersede),
        ] {
            assert_eq!(
                decide(running, requested),
                decision,
                "{running:?} then {requested:?}"
            );
        }
        assert!(Priority::Rebase > Priority::Fix);
        assert!(Priority::Fix > Priority::Review);
        assert_eq!(rebase.priority(), Priority::Rebase);
        assert_eq!(fix.priority(), Priority::Fix);
        assert_eq!(rebase.commit(), None);
        assert_eq!(fix.commit(), None);
    }
}
