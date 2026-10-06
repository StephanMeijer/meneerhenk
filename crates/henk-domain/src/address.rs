//! Addressing review feedback (§3.5): what a thread ends as, where in the
//! checkout a model may read and write, and when Henk may push at all. The
//! commit he pushes is in [`crate::commit`].

use std::fmt;

/// How one review thread ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadOutcome {
    /// The change is in Henk's commit.
    Fixed,
    /// Henk will not change it; the reply says why.
    Declined,
    /// Henk needs an answer first; the reply asks it.
    Question,
}

impl ThreadOutcome {
    /// Parses `fixed`, `declined` or `question`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "fixed" => Self::Fixed,
            "declined" => Self::Declined,
            "question" => Self::Question,
            _ => return None,
        })
    }
}

impl fmt::Display for ThreadOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fixed => "fixed",
            Self::Declined => "declined",
            Self::Question => "question",
        })
    }
}

/// Why a path is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// No path at all.
    #[error("a file path is required")]
    Empty,
    /// Absolute, or with a drive letter.
    #[error("{0:?} is absolute; paths are relative to the repository root")]
    Absolute(String),
    /// Climbs out with `..`.
    #[error("{0:?} leaves the repository")]
    Escapes(String),
    /// Inside `.git`.
    #[error("{0:?} is inside .git, which is Henk's, not the change's")]
    Git(String),
    /// A NUL or other control character.
    #[error("{0:?} has a control character")]
    Control(String),
}

/// A path inside the checkout, relative to its root and normalised:
/// `a/./b//c` is `a/b/c`. It can still be a symlink out of the checkout;
/// the tool that opens it checks that on the real file system.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// The repository root, for listing.
    #[must_use]
    pub fn root() -> Self {
        Self(String::new())
    }

    /// Parses a file path.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] for an empty, absolute, escaping, `.git` or
    /// control-character path.
    pub fn parse(value: &str) -> Result<Self, PathError> {
        let path = Self::parse_dir(value)?;
        if path.0.is_empty() {
            return Err(PathError::Empty);
        }
        Ok(path)
    }

    /// Parses a directory path; empty or `.` is the root.
    ///
    /// # Errors
    ///
    /// As [`Self::parse`], except that the root is allowed.
    pub fn parse_dir(value: &str) -> Result<Self, PathError> {
        let value = value.trim();
        if value.chars().any(char::is_control) {
            return Err(PathError::Control(value.to_owned()));
        }
        let unified = value.replace('\\', "/");
        let has_drive = unified.len() >= 2 && unified.as_bytes().get(1) == Some(&b':');
        if unified.starts_with('/') || has_drive {
            return Err(PathError::Absolute(value.to_owned()));
        }
        let mut parts = Vec::new();
        for part in unified.split('/') {
            match part {
                "" | "." => {}
                ".." => return Err(PathError::Escapes(value.to_owned())),
                part if part.eq_ignore_ascii_case(".git") => {
                    return Err(PathError::Git(value.to_owned()));
                }
                part => parts.push(part),
            }
        }
        Ok(Self(parts.join("/")))
    }

    /// The normalised path; empty for the root.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() { "." } else { &self.0 })
    }
}

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

    #[test]
    fn outcomes_parse() {
        assert_eq!(ThreadOutcome::parse(" Fixed "), Some(ThreadOutcome::Fixed));
        assert_eq!(
            ThreadOutcome::parse("declined"),
            Some(ThreadOutcome::Declined)
        );
        assert_eq!(
            ThreadOutcome::parse("question"),
            Some(ThreadOutcome::Question)
        );
        assert_eq!(ThreadOutcome::parse("ignored"), None);
    }

    #[test]
    fn paths_are_normalised_inside_the_checkout() {
        assert_eq!(
            WorkspacePath::parse("src/./lib.rs").unwrap().as_str(),
            "src/lib.rs"
        );
        assert_eq!(WorkspacePath::parse("a//b/").unwrap().as_str(), "a/b");
        assert_eq!(
            WorkspacePath::parse("dir\\file.rs").unwrap().as_str(),
            "dir/file.rs"
        );
        assert_eq!(WorkspacePath::parse_dir("").unwrap(), WorkspacePath::root());
        assert_eq!(WorkspacePath::parse_dir(".").unwrap().to_string(), ".");
        assert_eq!(
            WorkspacePath::parse(".github/ci.yml").unwrap().as_str(),
            ".github/ci.yml"
        );
    }

    #[test]
    fn paths_out_of_the_checkout_are_refused() {
        assert_eq!(WorkspacePath::parse(""), Err(PathError::Empty));
        assert_eq!(WorkspacePath::parse("."), Err(PathError::Empty));
        for path in ["/etc/passwd", "C:/x", "\\\\server\\share"] {
            assert!(
                matches!(WorkspacePath::parse(path), Err(PathError::Absolute(_))),
                "{path}"
            );
        }
        for path in ["../x", "a/../../x", "a/.."] {
            assert!(
                matches!(WorkspacePath::parse(path), Err(PathError::Escapes(_))),
                "{path}"
            );
        }
        for path in [".git/config", "sub/.git/hooks/pre-commit", ".GIT/HEAD"] {
            assert!(
                matches!(WorkspacePath::parse(path), Err(PathError::Git(_))),
                "{path}"
            );
        }
        assert!(matches!(
            WorkspacePath::parse("a\0b"),
            Err(PathError::Control(_))
        ));
    }

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
