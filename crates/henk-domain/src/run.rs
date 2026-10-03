//! Runs (§1.1): one execution of a review, a plan, a Discord turn or a mail reply.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a run id or model id was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The value is empty.
    #[error("value must not be empty")]
    Empty,
    /// The value contains whitespace, a control character or `-->`.
    #[error("value {0:?} contains whitespace, a control character or a comment terminator")]
    Invalid(String),
}

/// Validates a value that must survive inside a hidden marker (§8.6).
pub(crate) fn validate_token(value: &str) -> Result<(), TokenError> {
    if value.is_empty() {
        return Err(TokenError::Empty);
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) || value.contains("-->") {
        return Err(TokenError::Invalid(value.to_owned()));
    }
    Ok(())
}

/// The identifier of one run. Every run has a link (§1.1), and the id is
/// what the link, the summary and the hidden markers refer to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

impl RunId {
    /// Validates a run id. It must be non-empty and free of whitespace.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError`] for an empty or unprintable id.
    pub fn parse(value: impl Into<String>) -> Result<Self, TokenError> {
        let value = value.into();
        validate_token(&value)?;
        Ok(Self(value))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunId {
    type Error = TokenError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RunId> for String {
    fn from(id: RunId) -> Self {
        id.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identifier of one inbound event: something a hook received and the
/// bus delivered. Recorded locally next to the runs it led to (§8.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventId(String);

impl EventId {
    /// Validates an event id. It must be non-empty and free of whitespace.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError`] for an empty or unprintable id.
    pub fn parse(value: impl Into<String>) -> Result<Self, TokenError> {
        let value = value.into();
        validate_token(&value)?;
        Ok(Self(value))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventId {
    type Error = TokenError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<EventId> for String {
    fn from(id: EventId) -> Self {
        id.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The kinds of run (§1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    /// A code review of one commit (§3).
    Review,
    /// A plan for one issue (§4).
    Plan,
    /// One Discord turn (§5.2).
    DiscordTurn,
    /// One mail reply (§6).
    MailReply,
}

impl fmt::Display for RunKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Review => "review",
            Self::Plan => "plan",
            Self::DiscordTurn => "discord turn",
            Self::MailReply => "mail reply",
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn accepts_plain_ids() {
        assert_eq!(
            RunId::parse("run-2026-10-02-0001")
                .map(|id| id.to_string())
                .ok()
                .as_deref(),
            Some("run-2026-10-02-0001")
        );
    }

    #[test]
    fn rejects_empty_and_unprintable_ids() {
        assert_eq!(RunId::parse(""), Err(TokenError::Empty));
        assert!(matches!(RunId::parse("a b"), Err(TokenError::Invalid(_))));
        assert!(matches!(RunId::parse("a-->b"), Err(TokenError::Invalid(_))));
    }
}
