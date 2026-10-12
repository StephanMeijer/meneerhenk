//! Addressing review feedback (§3.5): what a review thread ends as.

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
}
