//! Hidden markers on everything Henk writes (§3.2, §5.3, §8.1, §8.6).
//!
//! A marker is an HTML comment at the end of a comment body. It is invisible
//! on GitHub and GitLab, and it records the run, the model and, when someone
//! asked for the write, for whom. Reading a marker back is how Henk
//! recognises his own words (§8.1) and how lanes see what is already there
//! (§3.2).

use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};

use crate::identity::DiscordUserId;
use crate::run::{RunId, TokenError, validate_token};

/// The name of a model, as the tools around the model identify it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelId(String);

impl ModelId {
    /// Validates a model id. It must be non-empty and free of whitespace.
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

impl TryFrom<String> for ModelId {
    type Error = TokenError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ModelId> for String {
    fn from(id: ModelId) -> Self {
        id.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const OPEN: &str = "<!-- meneer-henk";
const CLOSE: &str = "-->";

/// The hidden record on one of Henk's comments (§8.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The run that wrote the comment.
    pub run: RunId,
    /// The model that wrote the comment (§3.2).
    pub model: ModelId,
    /// Who asked for it, when the write came from Discord (§5.3).
    pub requested_by: Option<DiscordUserId>,
}

impl Marker {
    /// Renders the marker as an HTML comment.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{OPEN} run={} model={}", self.run, self.model);
        if let Some(user) = self.requested_by {
            let _ = write!(out, " for={user}");
        }
        out.push(' ');
        out.push_str(CLOSE);
        out
    }

    /// Appends the marker to a comment body, separated by a blank line.
    #[must_use]
    pub fn attach(&self, body: &str) -> String {
        let body = body.trim_end();
        if body.is_empty() {
            self.render()
        } else {
            format!("{body}\n\n{}", self.render())
        }
    }

    /// Finds and parses the first marker in a comment body.
    ///
    /// Returns `None` when there is no well-formed marker.
    #[must_use]
    pub fn parse(body: &str) -> Option<Self> {
        let start = body.find(OPEN)?;
        let rest = body.get(start + OPEN.len()..)?;
        let end = rest.find(CLOSE)?;
        let fields = rest.get(..end)?;

        let mut run = None;
        let mut model = None;
        let mut requested_by = None;
        for field in fields.split_whitespace() {
            let (key, value) = field.split_once('=')?;
            match key {
                "run" => run = RunId::parse(value).ok(),
                "model" => model = ModelId::parse(value).ok(),
                "for" => requested_by = Some(DiscordUserId::new(value.parse().ok()?)),
                _ => {}
            }
        }
        Some(Self {
            run: run?,
            model: model?,
            requested_by,
        })
    }

    /// Whether a comment body carries one of Henk's markers (§8.1).
    #[must_use]
    pub fn is_present(body: &str) -> bool {
        Self::parse(body).is_some()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn marker(requested_by: Option<u64>) -> Marker {
        Marker {
            run: RunId::parse("run-1").unwrap_or_else(|e| panic!("{e}")),
            model: ModelId::parse("lane-a/model-x").unwrap_or_else(|e| panic!("{e}")),
            requested_by: requested_by.map(DiscordUserId::new),
        }
    }

    #[test]
    fn renders_as_html_comment() {
        assert_eq!(
            marker(None).render(),
            "<!-- meneer-henk run=run-1 model=lane-a/model-x -->"
        );
        assert_eq!(
            marker(Some(42)).render(),
            "<!-- meneer-henk run=run-1 model=lane-a/model-x for=42 -->"
        );
    }

    #[test]
    fn round_trips_through_a_comment_body() {
        let body = marker(Some(42)).attach("Off by one on the last page.\n");
        assert!(body.starts_with("Off by one on the last page.\n\n<!--"));
        assert_eq!(Marker::parse(&body), Some(marker(Some(42))));
        assert!(Marker::is_present(&body));
    }

    #[test]
    fn ignores_bodies_without_a_marker() {
        assert_eq!(Marker::parse("No marker here."), None);
        assert_eq!(
            Marker::parse("<!-- meneer-henk run=x"),
            None,
            "unterminated"
        );
        assert_eq!(
            Marker::parse("<!-- meneer-henk model=x -->"),
            None,
            "missing run"
        );
        assert_eq!(
            Marker::parse("<!-- meneer-henk run=x model=y for=abc -->"),
            None,
            "bad id"
        );
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let parsed = Marker::parse("<!-- meneer-henk run=run-1 lane=3 model=lane-a/model-x -->");
        assert_eq!(parsed, Some(marker(None)));
    }
}
