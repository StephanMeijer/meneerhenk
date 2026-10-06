//! Triage fields a planner may fill (§4): effort, start and target date, and
//! health. Only empty fields are filled; a person's value is never replaced.

use std::fmt;

use time::Date;
use time::macros::format_description;

/// The largest weight a planner may set. GitLab weights are small integers;
/// anything near this is a typo, not an estimate.
pub const MAX_WEIGHT: u32 = 1000;

/// How an issue is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// On track.
    OnTrack,
    /// Needs attention.
    NeedsAttention,
    /// At risk.
    AtRisk,
}

impl Health {
    /// Parses the words a tool accepts: `on_track`, `needs_attention`, `at_risk`
    /// (GitLab's own `onTrack` spelling too).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(
            match value
                .trim()
                .to_ascii_lowercase()
                .replace(['-', ' '], "_")
                .as_str()
            {
                "on_track" | "ontrack" => Self::OnTrack,
                "needs_attention" | "needsattention" => Self::NeedsAttention,
                "at_risk" | "atrisk" => Self::AtRisk,
                _ => return None,
            },
        )
    }

    /// GitLab's name for it.
    #[must_use]
    pub fn gitlab_name(self) -> &'static str {
        match self {
            Self::OnTrack => "onTrack",
            Self::NeedsAttention => "needsAttention",
            Self::AtRisk => "atRisk",
        }
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OnTrack => "on track",
            Self::NeedsAttention => "needs attention",
            Self::AtRisk => "at risk",
        })
    }
}

/// Why fields cannot be set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TriageError {
    /// Nothing was asked for.
    #[error("no field given")]
    Empty,
    /// A date is not `YYYY-MM-DD`.
    #[error("{field} {value:?} is not a date in YYYY-MM-DD form")]
    BadDate {
        /// Which date.
        field: &'static str,
        /// What was given.
        value: String,
    },
    /// The due date would come before the start date.
    #[error("the due date {due} is before the start date {start}")]
    DueBeforeStart {
        /// Start.
        start: String,
        /// Due.
        due: String,
    },
    /// The weight is out of range.
    #[error("weight {0} is above {MAX_WEIGHT}")]
    WeightTooLarge(u64),
    /// The health is not one of the three.
    #[error("health {0:?} is not on_track, needs_attention or at_risk")]
    UnknownHealth(String),
    /// Fields that already have a value.
    #[error("already set, left alone: {}", .0.join(", "))]
    AlreadySet(Vec<&'static str>),
}

/// Triage fields: what an issue has, or what a planner wants to set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TriageFields {
    /// Effort.
    pub weight: Option<u32>,
    /// Start date.
    pub start: Option<Date>,
    /// Due date: the target date.
    pub due: Option<Date>,
    /// Health.
    pub health: Option<Health>,
}

/// Parses `YYYY-MM-DD`.
///
/// # Errors
///
/// Returns [`TriageError::BadDate`] naming `field`.
pub fn parse_date(field: &'static str, value: &str) -> Result<Date, TriageError> {
    Date::parse(value.trim(), format_description!("[year]-[month]-[day]")).map_err(|_| {
        TriageError::BadDate {
            field,
            value: value.to_owned(),
        }
    })
}

/// Formats a date as `YYYY-MM-DD`.
#[must_use]
pub fn date_text(date: Date) -> String {
    date.format(format_description!("[year]-[month]-[day]"))
        .unwrap_or_default()
}

impl TriageFields {
    /// Builds the fields a tool call asks for and checks each value.
    ///
    /// # Errors
    ///
    /// Returns a [`TriageError`] for the first unusable value, or
    /// [`TriageError::Empty`] when nothing is given.
    pub fn parse(
        weight: Option<u64>,
        start: Option<&str>,
        due: Option<&str>,
        health: Option<&str>,
    ) -> Result<Self, TriageError> {
        let weight = weight
            .map(|w| {
                u32::try_from(w)
                    .ok()
                    .filter(|w| *w <= MAX_WEIGHT)
                    .ok_or(TriageError::WeightTooLarge(w))
            })
            .transpose()?;
        let fields = Self {
            weight,
            start: start.map(|s| parse_date("start_date", s)).transpose()?,
            due: due.map(|d| parse_date("due_date", d)).transpose()?,
            health: health
                .map(|h| Health::parse(h).ok_or_else(|| TriageError::UnknownHealth(h.to_owned())))
                .transpose()?,
        };
        if fields.is_empty() {
            return Err(TriageError::Empty);
        }
        Ok(fields)
    }

    /// No field set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Checks that these fields may fill `current`: each one is empty there,
    /// and the dates, combined with what `current` already has, are in order.
    ///
    /// # Errors
    ///
    /// Returns [`TriageError::AlreadySet`] naming every field that has a value,
    /// or [`TriageError::DueBeforeStart`].
    pub fn may_fill(&self, current: &Self) -> Result<(), TriageError> {
        let taken: Vec<&'static str> = [
            ("weight", self.weight.is_some() && current.weight.is_some()),
            (
                "start_date",
                self.start.is_some() && current.start.is_some(),
            ),
            ("due_date", self.due.is_some() && current.due.is_some()),
            ("health", self.health.is_some() && current.health.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, taken)| taken.then_some(name))
        .collect();
        if !taken.is_empty() {
            return Err(TriageError::AlreadySet(taken));
        }
        if let (Some(start), Some(due)) = (self.start.or(current.start), self.due.or(current.due))
            && due < start
        {
            return Err(TriageError::DueBeforeStart {
                start: date_text(start),
                due: date_text(due),
            });
        }
        Ok(())
    }
}

impl fmt::Display for TriageFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(weight) = self.weight {
            parts.push(format!("weight {weight}"));
        }
        if let Some(start) = self.start {
            parts.push(format!("start {}", date_text(start)));
        }
        if let Some(due) = self.due {
            parts.push(format!("due {}", date_text(due)));
        }
        if let Some(health) = self.health {
            parts.push(format!("health {health}"));
        }
        f.write_str(&parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use time::macros::date;

    use super::*;

    #[test]
    fn health_parses_every_spelling() {
        for (text, health) in [
            ("on_track", Health::OnTrack),
            ("onTrack", Health::OnTrack),
            ("needs-attention", Health::NeedsAttention),
            (" AT RISK ", Health::AtRisk),
        ] {
            assert_eq!(Health::parse(text), Some(health), "{text}");
        }
        assert_eq!(Health::parse("fine"), None);
        assert_eq!(Health::AtRisk.gitlab_name(), "atRisk");
    }

    #[test]
    fn fields_parse_and_describe_themselves() {
        let fields = TriageFields::parse(
            Some(3),
            Some("2026-11-01"),
            Some("2026-11-30"),
            Some("on_track"),
        )
        .unwrap();
        assert_eq!(fields.weight, Some(3));
        assert_eq!(fields.start, Some(date!(2026 - 11 - 01)));
        assert_eq!(fields.due, Some(date!(2026 - 11 - 30)));
        assert_eq!(
            fields.to_string(),
            "weight 3, start 2026-11-01, due 2026-11-30, health on track"
        );
    }

    #[test]
    fn bad_values_are_refused() {
        assert_eq!(
            TriageFields::parse(None, None, None, None),
            Err(TriageError::Empty)
        );
        assert!(matches!(
            TriageFields::parse(None, None, Some("30-11-2026"), None),
            Err(TriageError::BadDate {
                field: "due_date",
                ..
            })
        ));
        assert!(matches!(
            TriageFields::parse(None, Some("2026-02-30"), None, None),
            Err(TriageError::BadDate {
                field: "start_date",
                ..
            })
        ));
        assert_eq!(
            TriageFields::parse(Some(1001), None, None, None),
            Err(TriageError::WeightTooLarge(1001))
        );
        assert_eq!(
            TriageFields::parse(Some(u64::MAX), None, None, None),
            Err(TriageError::WeightTooLarge(u64::MAX))
        );
        assert!(matches!(
            TriageFields::parse(None, None, None, Some("great")),
            Err(TriageError::UnknownHealth(_))
        ));
    }

    #[test]
    fn only_empty_fields_are_filled() {
        let current = TriageFields {
            weight: Some(5),
            health: Some(Health::AtRisk),
            ..TriageFields::default()
        };
        let wanted =
            TriageFields::parse(Some(3), None, Some("2026-12-01"), Some("on_track")).unwrap();
        assert_eq!(
            wanted.may_fill(&current),
            Err(TriageError::AlreadySet(vec!["weight", "health"]))
        );
        let due_only = TriageFields::parse(None, None, Some("2026-12-01"), None).unwrap();
        assert_eq!(due_only.may_fill(&current), Ok(()));
    }

    #[test]
    fn dates_stay_in_order_with_what_is_already_there() {
        let current = TriageFields {
            start: Some(date!(2026 - 12 - 10)),
            ..TriageFields::default()
        };
        let early_due = TriageFields::parse(None, None, Some("2026-12-01"), None).unwrap();
        assert!(matches!(
            early_due.may_fill(&current),
            Err(TriageError::DueBeforeStart { .. })
        ));
        let both = TriageFields::parse(None, Some("2026-12-02"), Some("2026-12-01"), None).unwrap();
        assert!(matches!(
            both.may_fill(&TriageFields::default()),
            Err(TriageError::DueBeforeStart { .. })
        ));
    }
}
