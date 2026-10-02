//! Overlapping reviews (open question 2, decided): one review per
//! pull/merge request at a time. A request for the commit already under
//! review joins it; a request for a newer commit supersedes it.

use crate::review::CommitSha;

/// What to do with a new review request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is running; start.
    Start,
    /// The same commit is under review; join that run.
    Join,
    /// Another commit is under review; cancel it and start.
    Supersede,
}

/// Decides, given the commit of the running review (if any) and the
/// commit requested now.
#[must_use]
pub fn decide(running: Option<&CommitSha>, requested: &CommitSha) -> Decision {
    match running {
        None => Decision::Start,
        Some(active) if active == requested => Decision::Join,
        Some(_) => Decision::Supersede,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn start_join_supersede() {
        let a = CommitSha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = CommitSha::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        assert_eq!(decide(None, &a), Decision::Start);
        assert_eq!(decide(Some(&a), &a), Decision::Join);
        assert_eq!(decide(Some(&a), &b), Decision::Supersede);
    }
}
