//! Paths inside a checkout: where in it a model may read and write.

use std::fmt;

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

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

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
}
