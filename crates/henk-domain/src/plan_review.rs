//! The points of a planner↔reviewer pairing (#328): what the reviewer
//! raises against a drafted plan, and the one answer the planner, or the
//! pairing's end, gives each.
//!
//! The reviewer may contest a rejection once, by raising the point again
//! with new evidence. A point still without an answer when the pairing ends
//! is `unsettled`. What the reviewer never saw answered, or what ended
//! unsettled, is still open, and the published plan says so.

use std::fmt::{self, Write as _};

/// A point's number within one run: `p1`, `p2`, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PointId(u32);

impl PointId {
    /// Reads `p3` (or `3`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.trim().strip_prefix('p').unwrap_or(text.trim());
        digits.parse().ok().filter(|n| *n > 0).map(Self)
    }
}

impl fmt::Display for PointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// What the reviewer raises against the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raised {
    /// The part of the plan or draft it is about: a step, a sub-issue, a
    /// label.
    pub about: String,
    /// What is wrong.
    pub claim: String,
    /// Why it matters, with the evidence.
    pub why: String,
    /// What would fix it.
    pub suggestion: String,
}

/// The one answer a point ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointAnswer {
    /// The planner took it and changed the draft.
    Taken {
        /// What changed.
        how: String,
    },
    /// The planner showed it does not hold.
    Rejected {
        /// Why.
        reason: String,
    },
    /// The pairing ended before the planner answered it.
    Unsettled {
        /// Why it ended.
        reason: String,
    },
}

impl PointAnswer {
    /// The stored word.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Taken { .. } => "taken",
            Self::Rejected { .. } => "rejected",
            Self::Unsettled { .. } => "unsettled",
        }
    }

    /// What changed, or why not.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Taken { how } => how,
            Self::Rejected { reason } | Self::Unsettled { reason } => reason,
        }
    }
}

/// One point and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPoint {
    /// Its number.
    pub id: PointId,
    /// The round it was raised in.
    pub round: u32,
    /// What the reviewer raised.
    pub raised: Raised,
    /// The rejected point it contests, if any.
    pub reopens: Option<PointId>,
    /// Its answer, once it has one.
    pub answer: Option<PointAnswer>,
}

impl PlanPoint {
    /// The point as the planner reads it.
    #[must_use]
    pub fn text(&self) -> String {
        let raised = &self.raised;
        let mut text = format!(
            "{} about {}: {}\n  why: {}",
            self.id, raised.about, raised.claim, raised.why
        );
        if !raised.suggestion.trim().is_empty() {
            let _ = write!(text, "\n  suggestion: {}", raised.suggestion);
        }
        if let Some(reopens) = self.reopens {
            let _ = write!(text, "\n  contests the rejection of {reopens}");
        }
        text
    }

    /// The point and its answer, in one line: `p1 about step 2 (it misses
    /// the migration): taken: step 2 adds it.`
    #[must_use]
    pub fn answer_line(&self) -> String {
        let fate = match &self.answer {
            None => "no answer".to_owned(),
            Some(answer) => format!("{}: {}", answer.as_str(), answer.text()),
        };
        format!(
            "{} about {} ({}): {fate}",
            self.id, self.raised.about, self.raised.claim
        )
    }
}

/// Why a planning pairing stopped, as the run records it. The words are
/// the review loop's, so a run's `loop_stop` reads the same for both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStop {
    /// The reviewer raised nothing new.
    Converged,
    /// The rounds ran out.
    MaxRounds(u32),
    /// The run's time limit, or a session's in a round.
    Timeout(String),
    /// A session broke off: a model error, a refusal, a turn limit.
    SessionFailed(String),
    /// Someone or something cancelled the run.
    Cancelled,
}

impl PlanStop {
    /// The stored word.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Converged => "converged",
            Self::MaxRounds(_) => "max_rounds",
            Self::Timeout(_) => "timeout",
            Self::SessionFailed(_) => "session_failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for PlanStop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Converged => f.write_str("converged, the reviewer raised nothing more"),
            Self::MaxRounds(rounds) => write!(f, "it reached max_rounds ({rounds})"),
            Self::Timeout(why) | Self::SessionFailed(why) => f.write_str(why),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// Why a raised point is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RaiseError {
    /// A field the reviewer left empty.
    Empty(&'static str),
    /// It contests a point there is none of.
    Unknown(PointId),
    /// It contests a point that was not rejected.
    NotRejected(PointId),
    /// The point was contested once already, or is itself a contest.
    Closed(PointId),
}

impl fmt::Display for RaiseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "{field} is required"),
            Self::Unknown(id) => write!(f, "there is no point {id}"),
            Self::NotRejected(id) => {
                write!(
                    f,
                    "{id} was not rejected; only a rejection can be contested"
                )
            }
            Self::Closed(id) => write!(
                f,
                "{id} was rejected twice and is closed; it cannot be contested again"
            ),
        }
    }
}

/// Why an answer is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// There is no such point.
    Unknown(PointId),
    /// It was raised in another round.
    NotThisRound(PointId),
    /// It has its answer already.
    Given(PointId),
    /// The answer needs its reason, or what changed.
    NoReason,
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(id) => write!(f, "there is no point {id}"),
            Self::NotThisRound(id) => write!(f, "{id} is not one of this round's points"),
            Self::Given(id) => write!(f, "{id} has its answer already"),
            Self::NoReason => f.write_str("an answer needs its reason, or what you changed"),
        }
    }
}

/// Every point of one pairing, in the order raised.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PointLedger {
    points: Vec<PlanPoint>,
    round: u32,
}

impl PointLedger {
    /// An empty ledger, before the first round.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts round `round`: points and answers from now on belong to it.
    pub fn start_round(&mut self, round: u32) {
        self.round = round;
    }

    /// The round under way.
    #[must_use]
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Adds a point of this round, or a contest of a rejected one.
    ///
    /// # Errors
    ///
    /// Returns [`RaiseError`] for an empty field, or a contest of a point
    /// that is unknown, was not rejected, or is closed.
    pub fn raise(
        &mut self,
        raised: Raised,
        reopens: Option<PointId>,
    ) -> Result<PointId, RaiseError> {
        for (field, value) in [
            ("about", &raised.about),
            ("claim", &raised.claim),
            ("why", &raised.why),
        ] {
            if value.trim().is_empty() {
                return Err(RaiseError::Empty(field));
            }
        }
        if let Some(target) = reopens {
            let found = self.get(target).ok_or(RaiseError::Unknown(target))?;
            if !matches!(found.answer, Some(PointAnswer::Rejected { .. })) {
                return Err(RaiseError::NotRejected(target));
            }
            let contested = self.points.iter().any(|p| p.reopens == Some(target));
            if found.reopens.is_some() || contested {
                return Err(RaiseError::Closed(target));
            }
        }
        let id = PointId(u32::try_from(self.points.len()).unwrap_or(u32::MAX - 1) + 1);
        self.points.push(PlanPoint {
            id,
            round: self.round,
            raised,
            reopens,
            answer: None,
        });
        Ok(id)
    }

    /// Gives a point of this round its answer.
    ///
    /// # Errors
    ///
    /// Returns [`AnswerError`] for an unknown point, one of another round,
    /// one that has its answer, or an answer without its text.
    pub fn answer(&mut self, id: PointId, answer: PointAnswer) -> Result<(), AnswerError> {
        if answer.text().trim().is_empty() {
            return Err(AnswerError::NoReason);
        }
        let round = self.round;
        let point = self
            .points
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or(AnswerError::Unknown(id))?;
        if point.round != round {
            return Err(AnswerError::NotThisRound(id));
        }
        if point.answer.is_some() {
            return Err(AnswerError::Given(id));
        }
        point.answer = Some(answer);
        Ok(())
    }

    /// A point by number.
    #[must_use]
    pub fn get(&self, id: PointId) -> Option<&PlanPoint> {
        self.points.iter().find(|p| p.id == id)
    }

    /// Every point, in the order raised.
    pub fn iter(&self) -> impl Iterator<Item = &PlanPoint> + '_ {
        self.points.iter()
    }

    /// The points raised in `round`.
    pub fn raised_in(&self, round: u32) -> impl Iterator<Item = &PlanPoint> + '_ {
        self.points.iter().filter(move |p| p.round == round)
    }

    /// This round's points still waiting for an answer.
    #[must_use]
    pub fn open(&self) -> Vec<&PlanPoint> {
        self.raised_in(self.round)
            .filter(|p| p.answer.is_none())
            .collect()
    }

    /// Every point without an answer becomes unsettled with `reason`;
    /// returns them.
    pub fn close_all(&mut self, reason: &str) -> Vec<PointId> {
        let mut closed = Vec::new();
        for point in &mut self.points {
            if point.answer.is_none() {
                point.answer = Some(PointAnswer::Unsettled {
                    reason: reason.to_owned(),
                });
                closed.push(point.id);
            }
        }
        closed
    }

    /// The points still open when the pairing ends: every unsettled one,
    /// and the last round's rejections, which the reviewer never saw. Once
    /// the reviewer read a rejection and raised nothing, it stands.
    #[must_use]
    pub fn still_open(&self) -> Vec<&PlanPoint> {
        let last = self.points.iter().map(|p| p.round).max().unwrap_or(0);
        let reviewed_after = self.round > last;
        self.points
            .iter()
            .filter(|p| match &p.answer {
                Some(PointAnswer::Unsettled { .. }) | None => true,
                Some(PointAnswer::Rejected { .. }) => p.round == last && !reviewed_after,
                Some(PointAnswer::Taken { .. }) => false,
            })
            .collect()
    }

    /// The section the published plan gets when the review did not end
    /// on its own (#328): why it stopped and the points still open. Empty
    /// when the reviewer raised nothing more.
    #[must_use]
    pub fn open_note(&self, stop: &PlanStop) -> String {
        if *stop == PlanStop::Converged {
            return String::new();
        }
        let mut text = format!(
            "### Points still open after review\n\nThe review stopped before the reviewer was done: {stop}."
        );
        let open = self.still_open();
        if open.is_empty() {
            text.push_str(" No point was left open.");
            return text;
        }
        text.push('\n');
        for point in open {
            let _ = write!(
                text,
                "\n- {} ({}): {}",
                point.raised.about, point.raised.claim, point.raised.why
            );
            if let Some(answer) = &point.answer {
                let _ = write!(text, " Planner: {} ({}).", answer.as_str(), answer.text());
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn raised(about: &str) -> Raised {
        Raised {
            about: about.to_owned(),
            claim: "it misses the migration".to_owned(),
            why: "the schema changes".to_owned(),
            suggestion: String::new(),
        }
    }

    fn rejected() -> PointAnswer {
        PointAnswer::Rejected {
            reason: "the schema stays".to_owned(),
        }
    }

    #[test]
    fn points_are_numbered_and_answered_once_in_their_round() {
        let mut ledger = PointLedger::new();
        ledger.start_round(1);
        let id = ledger.raise(raised("step 2"), None).unwrap();
        assert_eq!(id.to_string(), "p1");
        assert_eq!(PointId::parse("p1"), Some(id));
        assert_eq!(ledger.open().len(), 1);
        let taken = PointAnswer::Taken {
            how: "step 2 adds it".to_owned(),
        };
        ledger.answer(id, taken.clone()).unwrap();
        assert_eq!(ledger.answer(id, taken), Err(AnswerError::Given(id)));
        ledger.start_round(2);
        let next = ledger.raise(raised("step 3"), None).unwrap();
        ledger.start_round(3);
        assert_eq!(
            ledger.answer(next, rejected()),
            Err(AnswerError::NotThisRound(next))
        );
    }

    #[test]
    fn a_rejection_is_contested_once() {
        let mut ledger = PointLedger::new();
        ledger.start_round(1);
        let first = ledger.raise(raised("step 2"), None).unwrap();
        ledger.answer(first, rejected()).unwrap();
        ledger.start_round(2);
        let again = ledger.raise(raised("step 2"), Some(first)).unwrap();
        ledger.answer(again, rejected()).unwrap();
        ledger.start_round(3);
        assert_eq!(
            ledger.raise(raised("step 2"), Some(first)),
            Err(RaiseError::Closed(first))
        );
        assert_eq!(
            ledger.raise(raised("step 2"), Some(again)),
            Err(RaiseError::Closed(again))
        );
    }

    #[test]
    fn what_the_reviewer_never_saw_answered_stays_open() {
        let mut ledger = PointLedger::new();
        ledger.start_round(1);
        let seen = ledger.raise(raised("step 1"), None).unwrap();
        ledger.answer(seen, rejected()).unwrap();
        ledger.start_round(2);
        let last = ledger.raise(raised("step 2"), None).unwrap();
        ledger.answer(last, rejected()).unwrap();
        let unanswered = ledger.raise(raised("step 3"), None).unwrap();
        ledger.close_all("the rounds ran out");
        let open: Vec<PointId> = ledger.still_open().iter().map(|p| p.id).collect();
        assert_eq!(open, vec![last, unanswered]);
        let note = ledger.open_note(&PlanStop::MaxRounds(2));
        assert!(
            note.starts_with("### Points still open after review"),
            "{note}"
        );
        assert!(
            note.contains("- step 2 (it misses the migration)"),
            "{note}"
        );
        assert!(
            note.contains("Planner: unsettled (the rounds ran out)."),
            "{note}"
        );
        // A reviewer that read the answers and raised nothing settles them.
        assert!(
            note.contains("stopped before the reviewer was done: it reached max_rounds (2)."),
            "{note}"
        );
        ledger.start_round(3);
        let open: Vec<PointId> = ledger.still_open().iter().map(|p| p.id).collect();
        assert_eq!(open, vec![unanswered]);
        assert_eq!(ledger.open_note(&PlanStop::Converged), "");
    }

    #[test]
    fn a_point_needs_what_it_is_about_and_why() {
        let mut ledger = PointLedger::new();
        let mut empty = raised("");
        assert_eq!(
            ledger.raise(empty.clone(), None),
            Err(RaiseError::Empty("about"))
        );
        empty.about = "step 1".to_owned();
        empty.why = " ".to_owned();
        assert_eq!(ledger.raise(empty, None), Err(RaiseError::Empty("why")));
        assert_eq!(PlanStop::MaxRounds(3).as_str(), "max_rounds");
    }
}
