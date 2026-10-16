//! What a pull request needs next to be ready to merge (§4b, #637).
//!
//! A ticket run that keeps its pull request landable looks at the pull
//! request again and again. [`decide`] is the whole decision of one look: the
//! platform's word on whether it can merge, what the checks say, whether the
//! head is one Henk made and has reviewed, and how many times he has tried
//! already, in; the one thing to do next, out. It reads nothing and does
//! nothing, so every rule has a test.

use crate::review::CommitSha;

/// Whether the platform would merge the pull request now, and if not, why.
/// GitHub says it with `mergeable_state`, GitLab with
/// `detailed_merge_status`; the platform writers translate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeState {
    /// It could be merged now: no conflict, up to date where the base
    /// demands it, and what the base requires has passed.
    Mergeable,
    /// The base has moved and the base demands a branch that has caught up.
    NeedsRebase,
    /// The branch conflicts with the base.
    Conflicts,
    /// Checks the base requires are still running.
    Waiting,
    /// Something else stands in the way, such as a required review; the
    /// platform's word for it.
    Blocked(String),
    /// The platform has not worked it out yet.
    Unknown,
    /// It is a draft.
    Draft,
}

/// The pull request as the platform shows it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    /// Open, not merged or closed.
    pub open: bool,
    /// Merged into its base: the work landed.
    pub merged: bool,
    /// The head commit now.
    pub head: CommitSha,
    /// Whether it could merge.
    pub state: MergeState,
}

/// What the checks on the head say. Henk's own check is not among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiState {
    /// The head has no checks (after the time they would need to appear).
    None,
    /// Some are queued or running.
    Pending,
    /// All are done and none failed.
    Green,
    /// Some failed, were cancelled or ran out of time: their names.
    Red(Vec<String>),
}

/// Everything one look at the pull request found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// The pull request.
    pub standing: Standing,
    /// Its checks.
    pub ci: CiState,
    /// Whether the head is one Henk made, or was told about: not a person's
    /// push.
    pub head_is_ours: bool,
    /// Whether this repository lets Henk rebase (`rebase = false` turns it
    /// off).
    pub rebases_allowed: bool,
}

/// What the stage has done so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ledger {
    /// The head a review passed.
    pub reviewed: Option<CommitSha>,
    /// The head Henk's last rebase left: the platform may take a while to
    /// see it, and a second rebase of the same head would change nothing.
    pub rebased: Option<CommitSha>,
    /// Rebases made.
    pub rebases: u32,
    /// Fixes of failing checks made.
    pub ci_fixes: u32,
}

/// How many times the stage tries each thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Rebases in all.
    pub max_rebases: u32,
    /// Fixes of failing checks in all; 0 while Henk cannot make them.
    pub max_ci_fixes: u32,
}

/// Why the stage ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// Somebody else's work, or the pull request's own end, settled it; the
    /// stage did its job. Why.
    HandedOver(String),
    /// It cannot go on and a person has to look; why.
    Stuck(String),
}

/// The one thing to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The pull request is ready to merge.
    Ready,
    /// Rebase it onto its base.
    Rebase,
    /// Review the head.
    Review,
    /// Fix the failing checks, which are these.
    FixCi(Vec<String>),
    /// Nothing to do yet; what it waits for.
    Wait(String),
    /// Stop.
    End(End),
}

/// Decides what a pull request needs next. The order matters: a pull
/// request that is gone or taken over needs nothing from Henk; one that
/// needs a rebase is not worth a review before it; a review comes before the
/// checks are trusted, since a fix changes the head.
#[must_use]
pub fn decide(facts: &Facts, ledger: &Ledger, limits: &Limits) -> Action {
    let standing = &facts.standing;
    if standing.merged {
        return Action::End(End::HandedOver("it was merged".to_owned()));
    }
    if !standing.open {
        return Action::End(End::HandedOver("it was closed".to_owned()));
    }
    if !facts.head_is_ours {
        return Action::End(End::HandedOver(
            "somebody else pushed to its branch".to_owned(),
        ));
    }
    match &standing.state {
        MergeState::Draft => {
            return Action::End(End::HandedOver("it was made a draft".to_owned()));
        }
        MergeState::NeedsRebase | MergeState::Conflicts => {
            if ledger.rebased.as_ref() == Some(&standing.head) {
                return Action::Wait("the platform has not yet seen the rebased branch".to_owned());
            }
            if !facts.rebases_allowed {
                return Action::End(End::Stuck(
                    "it needs a rebase, and rebasing is off for this repository".to_owned(),
                ));
            }
            if ledger.rebases >= limits.max_rebases {
                return Action::End(End::Stuck(format!(
                    "it needed a rebase again after {} rebases",
                    ledger.rebases
                )));
            }
            return Action::Rebase;
        }
        _ => {}
    }
    if ledger.reviewed.as_ref() != Some(&standing.head) {
        return Action::Review;
    }
    if let CiState::Red(names) = &facts.ci {
        if ledger.ci_fixes < limits.max_ci_fixes {
            return Action::FixCi(names.clone());
        }
        return Action::End(End::Stuck(format!("checks failed: {}", names.join(", "))));
    }
    if facts.ci == CiState::Pending {
        return Action::Wait("checks are running".to_owned());
    }
    match &standing.state {
        MergeState::Mergeable => Action::Ready,
        MergeState::Waiting => Action::Wait("required checks are running".to_owned()),
        MergeState::Unknown => {
            Action::Wait("the platform is still working out whether it can merge".to_owned())
        }
        MergeState::Blocked(why) => Action::Wait(format!("blocked: {why}")),
        // Handled above.
        MergeState::NeedsRebase | MergeState::Conflicts | MergeState::Draft => {
            Action::Wait("waiting".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sha(digit: char) -> CommitSha {
        CommitSha::parse(&digit.to_string().repeat(40)).unwrap()
    }

    fn facts() -> Facts {
        Facts {
            standing: Standing {
                open: true,
                merged: false,
                head: sha('a'),
                state: MergeState::Mergeable,
            },
            ci: CiState::Green,
            head_is_ours: true,
            rebases_allowed: true,
        }
    }

    fn ledger() -> Ledger {
        Ledger {
            reviewed: Some(sha('a')),
            ..Ledger::default()
        }
    }

    const LIMITS: Limits = Limits {
        max_rebases: 3,
        max_ci_fixes: 0,
    };

    fn with_state(state: MergeState) -> Facts {
        let mut facts = facts();
        facts.standing.state = state;
        facts
    }

    #[test]
    fn a_reviewed_green_mergeable_head_is_ready() {
        assert_eq!(decide(&facts(), &ledger(), &LIMITS), Action::Ready);
        let no_ci = Facts {
            ci: CiState::None,
            ..facts()
        };
        assert_eq!(decide(&no_ci, &ledger(), &LIMITS), Action::Ready);
    }

    #[test]
    fn a_merged_or_closed_pull_request_ends_the_stage_well() {
        let mut merged = facts();
        merged.standing.merged = true;
        merged.standing.open = false;
        let mut closed = facts();
        closed.standing.open = false;
        for (case, facts) in [("merged", merged), ("closed", closed)] {
            assert!(
                matches!(
                    decide(&facts, &ledger(), &LIMITS),
                    Action::End(End::HandedOver(_))
                ),
                "{case}"
            );
        }
    }

    #[test]
    fn somebody_else_s_push_or_a_draft_hands_the_pull_request_over() {
        let foreign = Facts {
            head_is_ours: false,
            ..facts()
        };
        assert_eq!(
            decide(&foreign, &ledger(), &LIMITS),
            Action::End(End::HandedOver(
                "somebody else pushed to its branch".to_owned()
            ))
        );
        assert!(matches!(
            decide(&with_state(MergeState::Draft), &ledger(), &LIMITS),
            Action::End(End::HandedOver(_))
        ));
    }

    #[test]
    fn a_foreign_head_wins_over_everything_else() {
        let mut stale = with_state(MergeState::Conflicts);
        stale.head_is_ours = false;
        stale.ci = CiState::Red(vec!["test".to_owned()]);
        assert!(matches!(
            decide(&stale, &Ledger::default(), &LIMITS),
            Action::End(End::HandedOver(_))
        ));
    }

    #[test]
    fn a_branch_that_must_catch_up_or_conflicts_is_rebased_before_anything_else() {
        for state in [MergeState::NeedsRebase, MergeState::Conflicts] {
            let mut unreviewed = with_state(state.clone());
            unreviewed.ci = CiState::Red(vec!["test".to_owned()]);
            assert_eq!(
                decide(&unreviewed, &Ledger::default(), &LIMITS),
                Action::Rebase,
                "{state:?}: no review of a head that is about to change"
            );
        }
    }

    #[test]
    fn the_rebases_are_counted_and_can_be_switched_off() {
        let behind = with_state(MergeState::NeedsRebase);
        let used = Ledger {
            rebases: 3,
            ..ledger()
        };
        let Action::End(End::Stuck(why)) = decide(&behind, &used, &LIMITS) else {
            panic!("a fourth rebase");
        };
        assert!(why.contains("after 3 rebases"), "{why}");
        let one_left = Ledger {
            rebases: 2,
            ..ledger()
        };
        assert_eq!(decide(&behind, &one_left, &LIMITS), Action::Rebase);

        let off = Facts {
            rebases_allowed: false,
            ..behind
        };
        let Action::End(End::Stuck(why)) = decide(&off, &ledger(), &LIMITS) else {
            panic!("rebasing is off");
        };
        assert!(why.contains("off for this repository"), "{why}");
    }

    #[test]
    fn the_head_a_rebase_left_is_not_rebased_again_while_the_platform_catches_up() {
        let behind = with_state(MergeState::NeedsRebase);
        let rebased = Ledger {
            rebased: Some(sha('a')),
            rebases: 1,
            ..Ledger::default()
        };
        assert!(matches!(
            decide(&behind, &rebased, &LIMITS),
            Action::Wait(_)
        ));
        // A new head after that is rebased again.
        let mut moved = behind;
        moved.standing.head = sha('b');
        assert_eq!(decide(&moved, &rebased, &LIMITS), Action::Rebase);
    }

    #[test]
    fn a_head_no_review_passed_is_reviewed_before_its_checks_are_trusted() {
        let mut red = facts();
        red.ci = CiState::Red(vec!["test".to_owned()]);
        assert_eq!(decide(&red, &Ledger::default(), &LIMITS), Action::Review);
        let other = Ledger {
            reviewed: Some(sha('b')),
            ..Ledger::default()
        };
        assert_eq!(decide(&facts(), &other, &LIMITS), Action::Review);
    }

    #[test]
    fn failing_checks_are_fixed_within_the_limit_and_stop_the_stage_beyond_it() {
        let mut red = facts();
        red.ci = CiState::Red(vec!["test".to_owned(), "clippy".to_owned()]);
        let Action::End(End::Stuck(why)) = decide(&red, &ledger(), &LIMITS) else {
            panic!("no fixes allowed");
        };
        assert_eq!(why, "checks failed: test, clippy");

        let can_fix = Limits {
            max_ci_fixes: 3,
            ..LIMITS
        };
        assert_eq!(
            decide(&red, &ledger(), &can_fix),
            Action::FixCi(vec!["test".to_owned(), "clippy".to_owned()])
        );
        let spent = Ledger {
            ci_fixes: 3,
            ..ledger()
        };
        assert!(matches!(
            decide(&red, &spent, &can_fix),
            Action::End(End::Stuck(_))
        ));
    }

    #[test]
    fn it_waits_for_checks_and_for_the_platform_and_never_calls_that_ready() {
        let pending = Facts {
            ci: CiState::Pending,
            ..facts()
        };
        assert!(matches!(
            decide(&pending, &ledger(), &LIMITS),
            Action::Wait(_)
        ));
        for state in [
            MergeState::Waiting,
            MergeState::Unknown,
            MergeState::Blocked("a review is required".to_owned()),
        ] {
            let action = decide(&with_state(state.clone()), &ledger(), &LIMITS);
            assert!(matches!(action, Action::Wait(_)), "{state:?}: {action:?}");
        }
        assert_eq!(
            decide(
                &with_state(MergeState::Blocked("a review is required".to_owned())),
                &ledger(),
                &LIMITS
            ),
            Action::Wait("blocked: a review is required".to_owned())
        );
    }
}
