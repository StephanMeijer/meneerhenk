//! When Henk may push to a pull request at all (§3.5). The commit he
//! pushes is in [`crate::commit`].

/// What decides whether Henk may push to a pull request at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushFacts {
    /// Open, not merged or closed.
    pub open: bool,
    /// `owner/name` the pull request's branch lives in; `None` when that
    /// repository is gone.
    pub head_repo: Option<String>,
    /// `owner/name` the pull request targets.
    pub base_repo: String,
    /// The branch Henk would push to.
    pub head_ref: String,
    /// The repository's default branch.
    pub default_branch: String,
    /// Whether the branch is protected.
    pub head_protected: bool,
}

/// Why Henk will not push to this pull request, or `None` when he may (§3.5).
#[must_use]
pub fn push_refusal(facts: &PushFacts) -> Option<String> {
    if !facts.open {
        return Some("the pull request is not open".to_owned());
    }
    match &facts.head_repo {
        None => return Some("the pull request's branch no longer exists".to_owned()),
        Some(head) if !head.eq_ignore_ascii_case(&facts.base_repo) => {
            return Some(format!(
                "its branch is in another repository ({head}); I push only to branches of {}",
                facts.base_repo
            ));
        }
        Some(_) => {}
    }
    if facts.head_ref == facts.default_branch {
        return Some(format!(
            "its branch is the default branch ({}); I never push there",
            facts.head_ref
        ));
    }
    if facts.head_protected {
        return Some(format!(
            "its branch {} is protected; I never push to a protected branch",
            facts.head_ref
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn facts() -> PushFacts {
        PushFacts {
            open: true,
            head_repo: Some("o/r".to_owned()),
            base_repo: "o/r".to_owned(),
            head_ref: "feature".to_owned(),
            default_branch: "main".to_owned(),
            head_protected: false,
        }
    }

    #[test]
    fn henk_pushes_only_to_an_open_unprotected_branch_of_the_same_repository() {
        assert_eq!(push_refusal(&facts()), None);
        assert_eq!(
            push_refusal(&PushFacts {
                head_repo: Some("O/R".to_owned()),
                ..facts()
            }),
            None
        );
        for (case, refused) in [
            (
                "closed",
                PushFacts {
                    open: false,
                    ..facts()
                },
            ),
            (
                "fork",
                PushFacts {
                    head_repo: Some("fork/r".to_owned()),
                    ..facts()
                },
            ),
            (
                "gone",
                PushFacts {
                    head_repo: None,
                    ..facts()
                },
            ),
            (
                "default",
                PushFacts {
                    head_ref: "main".to_owned(),
                    ..facts()
                },
            ),
            (
                "protected",
                PushFacts {
                    head_protected: true,
                    ..facts()
                },
            ),
        ] {
            assert!(push_refusal(&refused).is_some(), "{case}");
        }
    }
}
