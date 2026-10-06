//! Skills: the team's own instructions for one kind of work, in the Agent
//! Skills format (a folder with a `SKILL.md`: front matter with a `name` and
//! a `description`, then Markdown). The system prompt lists names and
//! descriptions; an agent loads a body when its work calls for it.
//!
//! Only the two front-matter fields Henk needs are read, each on one line,
//! so no YAML parser is needed. Other keys (`license`, `allowed-tools`,
//! `metadata` and their indented lines) are skipped. A skill becomes prompt
//! text, so it is held to the style rules of §7.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::text::{StyleViolation, style_violations};

/// Characters a skill name may have at most.
pub const MAX_NAME_CHARS: usize = 64;

/// Characters a description may have at most.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Characters a skill body may have at most. A loaded skill stays in the
/// conversation, so this bounds what one costs every turn after.
pub const MAX_BODY_CHARS: usize = 20_000;

/// Why a skill name was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillNameError {
    /// The name is empty.
    #[error("a skill name must not be empty")]
    Empty,
    /// The name is longer than [`MAX_NAME_CHARS`].
    #[error("skill name {0:?} is longer than {MAX_NAME_CHARS} characters")]
    TooLong(String),
    /// The name has a character other than `a-z`, `0-9` and single `-`
    /// between them.
    #[error("skill name {0:?} may hold only a-z, 0-9 and single hyphens between them")]
    Invalid(String),
}

/// A skill's name: lowercase letters, digits and single hyphens between
/// them, as the folder it lives in is named.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillName(String);

impl SkillName {
    /// Validates a skill name.
    ///
    /// # Errors
    ///
    /// Returns [`SkillNameError`] for an empty, overlong or malformed name.
    pub fn parse(value: impl Into<String>) -> Result<Self, SkillNameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SkillNameError::Empty);
        }
        if value.chars().count() > MAX_NAME_CHARS {
            return Err(SkillNameError::TooLong(value));
        }
        let charset = value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !charset || value.starts_with('-') || value.ends_with('-') || value.contains("--") {
            return Err(SkillNameError::Invalid(value));
        }
        Ok(Self(value))
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillName {
    type Error = SkillNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<SkillName> for String {
    fn from(name: SkillName) -> Self {
        name.0
    }
}

impl fmt::Display for SkillName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a `SKILL.md` was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    /// The text does not start with a `---` line.
    #[error("SKILL.md must start with a front matter block between --- lines")]
    NoFrontMatter,
    /// The front matter has no closing `---` line.
    #[error("the front matter has no closing --- line")]
    Unterminated,
    /// A front matter line is not `key: value` or an indented line.
    #[error("front matter line {0:?} is not key: value")]
    Malformed(String),
    /// A field Henk reads spans several lines.
    #[error("{0} must be a single line")]
    MultiLine(&'static str),
    /// A field Henk reads appears twice.
    #[error("{0} appears twice")]
    Duplicate(&'static str),
    /// A required field is missing or empty.
    #[error("{0} is missing")]
    Missing(&'static str),
    /// The name is not a valid skill name.
    #[error(transparent)]
    Name(#[from] SkillNameError),
    /// The name differs from the folder the skill is in.
    #[error("name {name:?} differs from its folder {folder:?}")]
    NameMismatch {
        /// The name in the front matter.
        name: String,
        /// The folder's name.
        folder: String,
    },
    /// The description is longer than [`MAX_DESCRIPTION_CHARS`].
    #[error("the description has {0} characters; at most {MAX_DESCRIPTION_CHARS}")]
    DescriptionTooLong(usize),
    /// Nothing follows the front matter.
    #[error("the skill has no instructions after its front matter")]
    EmptyBody,
    /// The body is longer than [`MAX_BODY_CHARS`].
    #[error("the instructions have {0} characters; at most {MAX_BODY_CHARS}")]
    BodyTooLong(usize),
    /// The text breaks a style rule (§7).
    #[error("{part} breaks the style rules: {violation:?}")]
    Style {
        /// `description` or `instructions`.
        part: &'static str,
        /// The first violation found.
        violation: StyleViolation,
    },
}

/// One skill: what it is for, and the instructions themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    name: SkillName,
    description: String,
    body: String,
}

impl Skill {
    /// Parses the text of `<folder>/SKILL.md`.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError`] when the front matter is missing or malformed,
    /// a field is missing, the name differs from `folder`, a limit is
    /// exceeded or the text breaks a style rule.
    pub fn parse(folder: &str, text: &str) -> Result<Self, SkillError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.lines();
        if lines.next().map(str::trim_end) != Some("---") {
            return Err(SkillError::NoFrontMatter);
        }
        let mut front = Vec::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                closed = true;
                break;
            }
            front.push(line);
        }
        if !closed {
            return Err(SkillError::Unterminated);
        }
        let (name, description) = read_front_matter(&front)?;

        let name = SkillName::parse(name)?;
        if name.as_str() != folder {
            return Err(SkillError::NameMismatch {
                name: name.0,
                folder: folder.to_owned(),
            });
        }
        let description_chars = description.chars().count();
        if description_chars > MAX_DESCRIPTION_CHARS {
            return Err(SkillError::DescriptionTooLong(description_chars));
        }
        let body = lines.collect::<Vec<_>>().join("\n").trim().to_owned();
        if body.is_empty() {
            return Err(SkillError::EmptyBody);
        }
        let body_chars = body.chars().count();
        if body_chars > MAX_BODY_CHARS {
            return Err(SkillError::BodyTooLong(body_chars));
        }
        for (part, text) in [("description", &description), ("instructions", &body)] {
            if let Some(violation) = style_violations(text).into_iter().next() {
                return Err(SkillError::Style { part, violation });
            }
        }
        Ok(Self {
            name,
            description,
            body,
        })
    }

    /// The skill's name.
    #[must_use]
    pub fn name(&self) -> &SkillName {
        &self.name
    }

    /// When to use the skill, as the catalogue shows it.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The instructions.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// Reads `name` and `description` from the front matter lines, skipping
/// every other key and the indented lines under it.
fn read_front_matter(lines: &[&str]) -> Result<(String, String), SkillError> {
    let mut name = None;
    let mut description = None;
    // The field the previous top-level line set, if it is one Henk reads.
    let mut current: Option<&'static str> = None;
    for line in lines {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(field) = current {
                return Err(SkillError::MultiLine(field));
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(SkillError::Malformed((*line).to_owned()));
        };
        let (field, slot) = match key.trim() {
            "name" => ("name", &mut name),
            "description" => ("description", &mut description),
            _ => {
                current = None;
                continue;
            }
        };
        let value = value.trim();
        if value.starts_with(['>', '|']) {
            return Err(SkillError::MultiLine(field));
        }
        if slot.is_some() {
            return Err(SkillError::Duplicate(field));
        }
        *slot = Some(unquote(value).trim().to_owned());
        current = Some(field);
    }
    let name = name
        .filter(|n| !n.is_empty())
        .ok_or(SkillError::Missing("name"))?;
    let description = description
        .filter(|d| !d.is_empty())
        .ok_or(SkillError::Missing("description"))?;
    Ok((name, description))
}

/// Strips one pair of matching single or double quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// The catalogue a system prompt shows: one `- name: description` line per
/// skill, sorted by name so the prompt is the same on every run.
#[must_use]
pub fn render_catalogue<'a>(skills: impl IntoIterator<Item = &'a Skill>) -> String {
    let mut skills: Vec<&Skill> = skills.into_iter().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    const SQL: &str = "---\nname: sql-migrations\ndescription: Use when a change adds or edits a SQL migration.\n---\n\n# SQL migrations\n\nEvery migration must be reversible.\n";

    #[test]
    fn a_skill_is_parsed() {
        let skill = Skill::parse("sql-migrations", SQL).unwrap();
        assert_eq!(skill.name().as_str(), "sql-migrations");
        assert_eq!(
            skill.description(),
            "Use when a change adds or edits a SQL migration."
        );
        assert_eq!(
            skill.body(),
            "# SQL migrations\n\nEvery migration must be reversible."
        );
    }

    #[test]
    fn quotes_crlf_and_other_keys_are_handled() {
        let text = "---\r\nname: \"rust-errors\"\r\nlicense: MIT\r\nallowed-tools: Read Grep\r\nmetadata:\r\n  author: team\r\n  version: \"1\"\r\ndescription: 'Our error rules: typed, never unwrap.'\r\n# a comment\r\n---\r\nReturn typed errors.\r\n";
        let skill = Skill::parse("rust-errors", text).unwrap();
        assert_eq!(skill.description(), "Our error rules: typed, never unwrap.");
        assert_eq!(skill.body(), "Return typed errors.");
    }

    #[test]
    fn malformed_front_matter_is_refused() {
        let cases = [
            (
                "no front matter",
                "# Just markdown",
                SkillError::NoFrontMatter,
            ),
            (
                "unterminated",
                "---\nname: a\ndescription: b\n",
                SkillError::Unterminated,
            ),
            (
                "not key: value",
                "---\nname a\n---\nx",
                SkillError::Malformed("name a".to_owned()),
            ),
            (
                "folded description",
                "---\nname: a\ndescription: >\n  long\n---\nx",
                SkillError::MultiLine("description"),
            ),
            (
                "continued name",
                "---\nname: a\n  b\ndescription: d\n---\nx",
                SkillError::MultiLine("name"),
            ),
            (
                "duplicate",
                "---\nname: a\nname: a\ndescription: d\n---\nx",
                SkillError::Duplicate("name"),
            ),
            (
                "no name",
                "---\ndescription: d\n---\nx",
                SkillError::Missing("name"),
            ),
            (
                "empty description",
                "---\nname: a\ndescription: \"\"\n---\nx",
                SkillError::Missing("description"),
            ),
            (
                "no body",
                "---\nname: a\ndescription: d\n---\n\n",
                SkillError::EmptyBody,
            ),
        ];
        for (what, text, expected) in cases {
            assert_eq!(Skill::parse("a", text).unwrap_err(), expected, "{what}");
        }
    }

    #[test]
    fn the_name_must_be_valid_and_match_its_folder() {
        assert_eq!(
            Skill::parse("other", SQL).unwrap_err(),
            SkillError::NameMismatch {
                name: "sql-migrations".to_owned(),
                folder: "other".to_owned()
            }
        );
        let bad = "---\nname: SQL Migrations\ndescription: d\n---\nx";
        assert!(matches!(
            Skill::parse("SQL Migrations", bad).unwrap_err(),
            SkillError::Name(SkillNameError::Invalid(_))
        ));
        for name in ["a", "a-b", "rust2", "x1-y2-z3"] {
            assert!(SkillName::parse(name).is_ok(), "{name}");
        }
        for name in ["", "-a", "a-", "a--b", "A", "a_b", "a b", "é"] {
            assert!(SkillName::parse(name).is_err(), "{name:?}");
        }
        assert!(SkillName::parse("a".repeat(MAX_NAME_CHARS)).is_ok());
        assert!(SkillName::parse("a".repeat(MAX_NAME_CHARS + 1)).is_err());
    }

    #[test]
    fn limits_and_style_are_enforced() {
        let long_description = format!(
            "---\nname: a\ndescription: {}\n---\nx",
            "d".repeat(MAX_DESCRIPTION_CHARS + 1)
        );
        assert_eq!(
            Skill::parse("a", &long_description).unwrap_err(),
            SkillError::DescriptionTooLong(MAX_DESCRIPTION_CHARS + 1)
        );
        let long_body = format!(
            "---\nname: a\ndescription: d\n---\n{}",
            "b".repeat(MAX_BODY_CHARS + 1)
        );
        assert_eq!(
            Skill::parse("a", &long_body).unwrap_err(),
            SkillError::BodyTooLong(MAX_BODY_CHARS + 1)
        );
        let dash = "---\nname: a\ndescription: d\n---\nNever do this \u{2014} ever.";
        assert!(matches!(
            Skill::parse("a", dash).unwrap_err(),
            SkillError::Style {
                part: "instructions",
                violation: StyleViolation::EmDash { .. }
            }
        ));
    }

    #[test]
    fn the_catalogue_is_sorted_by_name() {
        let b = Skill::parse(
            "b-skill",
            "---\nname: b-skill\ndescription: Second.\n---\nx",
        )
        .unwrap();
        let a = Skill::parse("a-skill", "---\nname: a-skill\ndescription: First.\n---\nx").unwrap();
        assert_eq!(
            render_catalogue([&b, &a]),
            "- a-skill: First.\n- b-skill: Second."
        );
    }
}
