//! One run per pull/merge request at a time (open question 2, decided;
//! #580). A review request for the commit already under review joins it;
//! one for a newer commit supersedes it. A rebase changes the commits
//! everything else looks at, so it supersedes a review of the pull
//! request, and a review asked for meanwhile waits until the rebase ends.
//! A second rebase request joins the first.

use crate::review::CommitSha;

/// What a run does to a pull request, as the queue tells runs apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// A review of this commit.
    Review(CommitSha),
    /// A rebase onto the base branch (#579).
    Rebase,
}

impl Work {
    /// Where it waits in the queue for a slot.
    #[must_use]
    pub const fn priority(&self) -> Priority {
        match self {
            Self::Review(_) => Priority::Review,
            Self::Rebase => Priority::Rebase,
        }
    }

    /// The commit it reviews, for a review.
    #[must_use]
    pub const fn commit(&self) -> Option<&CommitSha> {
        match self {
            Self::Review(commit) => Some(commit),
            Self::Rebase => None,
        }
    }
}

/// The order runs take free slots in: a higher priority first, then the
/// order they asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// A review.
    Review,
    /// A rebase: ahead of everything else.
    Rebase,
}

impl Priority {
    /// The word for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
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
    /// A rebase is running; start once it has ended, unless it pushed a
    /// new head, whose review then takes this one's place.
    After,
}

/// Decides, given the work running on the pull request (if any) and the
/// work requested now.
#[must_use]
pub fn decide(running: Option<&Work>, requested: &Work) -> Decision {
    match (running, requested) {
        (None, _) => Decision::Start,
        (Some(Work::Rebase), Work::Rebase) => Decision::Join,
        (Some(Work::Rebase), Work::Review(_)) => Decision::After,
        (Some(Work::Review(active)), Work::Review(asked)) if active == asked => Decision::Join,
        // A newer commit, or a rebase that replaces the commit.
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
        for (running, requested, decision) in [
            (None, &a, Decision::Start),
            (None, &rebase, Decision::Start),
            (Some(&a), &a, Decision::Join),
            (Some(&a), &b, Decision::Supersede),
            (Some(&a), &rebase, Decision::Supersede),
            (Some(&rebase), &a, Decision::After),
            (Some(&rebase), &rebase, Decision::Join),
        ] {
            assert_eq!(
                decide(running, requested),
                decision,
                "{running:?} then {requested:?}"
            );
        }
        assert!(Priority::Rebase > Priority::Review);
        assert_eq!(rebase.priority(), Priority::Rebase);
        assert_eq!(rebase.commit(), None);
    }
}
