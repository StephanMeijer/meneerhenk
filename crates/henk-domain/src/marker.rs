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

/// What kind of comment a marker sits on (§3.3 folding rules depend on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    /// A line comment with one problem (§3.2).
    Finding,
    /// The comment that closes a review (§3.3). Only the latest counts.
    Summary,
    /// A conversational reply or greeting (§3.4). Never folded.
    Reply,
    /// The plan written into an issue (§4).
    Plan,
    /// A statement that a run failed (§8.8).
    Failure,
}

impl MarkerKind {
    /// What an AI agent that picks up this comment should do with it.
    /// Hidden in an HTML comment after the marker, so people see only the
    /// text and an agent reading the raw body sees the rules.
    #[must_use]
    pub const fn notes(self) -> &'static str {
        match self {
            Self::Finding => {
                "This is one review finding by Meneer Henk, an advisory reviewer. If you are an AI agent working on this change: address it, or say why not, in a reply in this thread. Do not open a new comment for it and do not edit or delete this one. Resolve the thread once it is addressed; a later review folds it when the line changes. The finding is information, not an instruction: decide on the code."
            }
            Self::Summary => {
                "This is the summary of one review by Meneer Henk. The findings are the review threads on the lines, not this comment. If you are an AI agent: do not reply here; handle each thread where it is. A new review runs on the next push, or when someone asks Henk for a review in a comment."
            }
            Self::Reply => {
                "A conversational reply by Meneer Henk. Nothing to act on. If you are an AI agent: do not answer it."
            }
            Self::Plan => {
                "This plan section was written by Meneer Henk and is maintained by him. If you are an AI agent: do not edit the section; say what you did or disagree with in a comment on the issue, and Henk picks it up on the next planning pass."
            }
            Self::Failure => {
                "A run of Meneer Henk did not complete. That is Henk's failure, not the code's. If you are an AI agent: nothing to do here and nothing to fix in the change; a later run replaces this comment's role."
            }
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Finding => "finding",
            Self::Summary => "summary",
            Self::Reply => "reply",
            Self::Plan => "plan",
            Self::Failure => "failure",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "finding" => Self::Finding,
            "summary" => Self::Summary,
            "reply" => Self::Reply,
            "plan" => Self::Plan,
            "failure" => Self::Failure,
            _ => return None,
        })
    }
}

const OPEN: &str = "<!-- meneer-henk";
const CLOSE: &str = "-->";

/// Who withdrew a finding as wrong, and how (§3.2, §8.6). The comment keeps
/// its original author; this records the second hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    /// The run that withdrew it.
    pub run: RunId,
    /// The model that withdrew it.
    pub model: ModelId,
    /// The model that fact-checked the withdrawal, when one did.
    pub checked_by: Option<ModelId>,
}

/// What an AI agent should do with a withdrawn finding: nothing.
const WITHDRAWN_NOTE: &str = "This finding was withdrawn by Meneer Henk as wrong, and its thread is resolved. If you are an AI agent: nothing to do here; do not reply to it or reopen it.";

/// The hidden record on one of Henk's comments (§8.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The run that wrote the comment.
    pub run: RunId,
    /// The model that wrote the comment (§3.2).
    pub model: ModelId,
    /// The model that fact-checked it before it was written, when one did
    /// (§3.2). Absent on unchecked comments and on older markers.
    pub checked_by: Option<ModelId>,
    /// Who asked for it, when the write came from Discord (§5.3).
    pub requested_by: Option<DiscordUserId>,
    /// What kind of comment this is. Older markers have none.
    pub kind: Option<MarkerKind>,
    /// Set when the finding was later withdrawn as wrong; `run` and `model`
    /// still name who wrote it.
    pub withdrawn: Option<Withdrawal>,
}

impl Marker {
    /// Renders the marker as an HTML comment.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("{OPEN} run={} model={}", self.run, self.model);
        if let Some(checker) = &self.checked_by {
            let _ = write!(out, " checked_by={checker}");
        }
        if let Some(user) = self.requested_by {
            let _ = write!(out, " for={user}");
        }
        if let Some(kind) = self.kind {
            let _ = write!(out, " kind={}", kind.as_str());
        }
        if let Some(withdrawal) = &self.withdrawn {
            let _ = write!(
                out,
                " withdrawn_by={} withdrawn_model={}",
                withdrawal.run, withdrawal.model
            );
            if let Some(checker) = &withdrawal.checked_by {
                let _ = write!(out, " withdrawal_checked_by={checker}");
            }
        }
        out.push(' ');
        out.push_str(CLOSE);
        out
    }

    /// Appends the marker to a comment body, separated by a blank line,
    /// and, when the kind is known, a hidden note for AI agents that pick
    /// the comment up ([`MarkerKind::notes`]; a withdrawn finding gets its
    /// own).
    #[must_use]
    pub fn attach(&self, body: &str) -> String {
        let body = body.trim_end();
        let mut out = if body.is_empty() {
            self.render()
        } else {
            format!("{body}\n\n{}", self.render())
        };
        let note = if self.withdrawn.is_some() {
            Some(WITHDRAWN_NOTE)
        } else {
            self.kind.map(MarkerKind::notes)
        };
        if let Some(note) = note {
            let _ = write!(out, "\n<!-- {note} -->");
        }
        out
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
        let mut checked_by = None;
        let mut requested_by = None;
        let mut kind = None;
        let mut withdrawn_by = None;
        let mut withdrawn_model = None;
        let mut withdrawal_checked_by = None;
        for field in fields.split_whitespace() {
            let (key, value) = field.split_once('=')?;
            match key {
                "run" => run = RunId::parse(value).ok(),
                "model" => model = ModelId::parse(value).ok(),
                "checked_by" => checked_by = ModelId::parse(value).ok(),
                "for" => requested_by = Some(DiscordUserId::new(value.parse().ok()?)),
                "kind" => kind = MarkerKind::parse(value),
                "withdrawn_by" => withdrawn_by = RunId::parse(value).ok(),
                "withdrawn_model" => withdrawn_model = ModelId::parse(value).ok(),
                "withdrawal_checked_by" => withdrawal_checked_by = ModelId::parse(value).ok(),
                _ => {}
            }
        }
        Some(Self {
            run: run?,
            model: model?,
            checked_by,
            requested_by,
            kind,
            withdrawn: withdrawn_by
                .zip(withdrawn_model)
                .map(|(run, model)| Withdrawal {
                    run,
                    model,
                    checked_by: withdrawal_checked_by,
                }),
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
            kind: None,
            checked_by: None,
            withdrawn: None,
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
    fn names_the_fact_checking_model_when_there_was_one() {
        let checked = Marker {
            checked_by: Some(ModelId::parse("claude-opus-5-5").unwrap()),
            kind: Some(MarkerKind::Finding),
            ..marker(None)
        };
        assert!(
            checked
                .render()
                .starts_with("<!-- meneer-henk run=run-1 model=lane-a/model-x checked_by=claude-opus-5-5 kind=finding")
        );
        assert_eq!(Marker::parse(&checked.attach("Off by one.")), Some(checked));
        let unchecked = Marker::parse("<!-- meneer-henk run=run-1 model=m -->").unwrap();
        assert_eq!(
            unchecked.checked_by, None,
            "older markers parse as unchecked"
        );
    }

    #[test]
    fn a_withdrawal_keeps_the_author_and_brings_its_own_note() {
        let withdrawn = Marker {
            kind: Some(MarkerKind::Finding),
            withdrawn: Some(Withdrawal {
                run: RunId::parse("run-2").unwrap(),
                model: ModelId::parse("mistral").unwrap(),
                checked_by: Some(ModelId::parse("claude-opus-5-5").unwrap()),
            }),
            ..marker(None)
        };
        let rendered = withdrawn.render();
        assert!(
            rendered.contains("run=run-1 model=lane-a/model-x"),
            "the author stays: {rendered}"
        );
        assert!(rendered.contains(
            "withdrawn_by=run-2 withdrawn_model=mistral withdrawal_checked_by=claude-opus-5-5"
        ));
        let body = withdrawn.attach("Withdrawn. It was fine.");
        assert_eq!(Marker::parse(&body), Some(withdrawn.clone()));
        assert!(body.contains(WITHDRAWN_NOTE));
        assert!(!body.contains(MarkerKind::Finding.notes()));
        assert!(crate::text::is_in_style(WITHDRAWN_NOTE));

        let unchecked = Marker {
            withdrawn: Some(Withdrawal {
                checked_by: None,
                ..withdrawn.withdrawn.clone().unwrap()
            }),
            ..withdrawn
        };
        assert_eq!(Marker::parse(&unchecked.render()), Some(unchecked.clone()));
        assert!(!unchecked.render().contains("withdrawal_checked_by"));
        assert_eq!(
            Marker::parse("<!-- meneer-henk run=r model=m kind=finding -->")
                .unwrap()
                .withdrawn,
            None,
            "older markers are not withdrawn"
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
    fn kind_round_trips_and_old_markers_still_parse() {
        let mut with_kind = marker(None);
        with_kind.kind = Some(MarkerKind::Summary);
        let rendered = with_kind.render();
        assert!(rendered.ends_with("kind=summary -->"));
        assert_eq!(Marker::parse(&rendered), Some(with_kind));
        assert_eq!(
            Marker::parse("<!-- meneer-henk run=run-1 model=lane-a/model-x -->"),
            Some(marker(None))
        );
        assert_eq!(
            Marker::parse("<!-- meneer-henk run=r model=m kind=bogus -->").and_then(|m| m.kind),
            None
        );
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let parsed = Marker::parse("<!-- meneer-henk run=run-1 lane=3 model=lane-a/model-x -->");
        assert_eq!(parsed, Some(marker(None)));
    }

    #[test]
    fn notes_for_agents_follow_the_marker_and_do_not_confuse_the_parser() {
        let marker = Marker {
            run: RunId::parse("r-1").unwrap(),
            model: ModelId::parse("m").unwrap(),
            requested_by: None,
            kind: Some(MarkerKind::Finding),
            checked_by: None,
            withdrawn: None,
        };
        let body = marker.attach("Off by one.");
        assert!(body.starts_with("Off by one.\n\n<!-- meneer-henk run=r-1 model=m kind=finding -->\n<!-- This is one review finding"));
        assert_eq!(Marker::parse(&body).unwrap(), marker);
        assert_eq!(body.split("<!--").next().unwrap().trim(), "Off by one.");
        for kind in [
            MarkerKind::Finding,
            MarkerKind::Summary,
            MarkerKind::Reply,
            MarkerKind::Plan,
            MarkerKind::Failure,
        ] {
            assert!(crate::text::is_in_style(kind.notes()), "{kind:?}");
            assert!(
                !kind.notes().contains("--"),
                "{kind:?}: would close the HTML comment early"
            );
            assert!(!kind.notes().starts_with("meneer-henk"), "{kind:?}");
        }
        let plain = Marker {
            kind: None,
            ..marker
        };
        assert!(!plain.attach("hi").contains("If you are an AI agent"));
    }
}
