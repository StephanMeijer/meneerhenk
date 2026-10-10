//! The findings of a reviewer↔fixer loop (#285): what the reviewer reports,
//! and the one verdict the fixer, or the loop's end, gives each.
//!
//! The reviewer may contest a rejection once, by reporting the finding
//! again with new evidence; a finding rejected twice is closed. A `fixed`
//! verdict holds once Henk pushed the commit; when nothing was pushed it
//! becomes `unsettled`, as does every finding still open when the run ends.

use std::fmt;

/// A finding's number within one run: `f1`, `f2`, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FindingId(u32);

impl FindingId {
    /// Reads `f3` (or `3`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.trim().strip_prefix('f').unwrap_or(text.trim());
        digits.parse().ok().filter(|n| *n > 0).map(Self)
    }
}

impl fmt::Display for FindingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "f{}", self.0)
    }
}

/// What the reviewer reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// File path as in the diff.
    pub path: String,
    /// Line in the new version of the file.
    pub line: u32,
    /// What is wrong.
    pub claim: String,
    /// Why it matters.
    pub why: String,
    /// What would fix it.
    pub fix: String,
}

/// The one verdict a finding ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopVerdict {
    /// The fixer fixed it; `commit` once Henk pushed the fix.
    Fixed {
        /// What the fixer changed.
        what: String,
        /// The pushed commit.
        commit: Option<String>,
    },
    /// The fixer showed it does not hold.
    Rejected {
        /// Why, naming the code that shows it.
        reason: String,
    },
    /// It holds, but the fixer will not fix it here.
    WontFix {
        /// Why, such as out of the pull request's scope.
        reason: String,
    },
    /// No verdict held: none was given, or a fix was never pushed.
    Unsettled {
        /// Why.
        reason: String,
    },
}

impl LoopVerdict {
    /// The stored word.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fixed { .. } => "fixed",
            Self::Rejected { .. } => "rejected",
            Self::WontFix { .. } => "wont_fix",
            Self::Unsettled { .. } => "unsettled",
        }
    }

    /// The text that goes with it: what changed, or why.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Fixed { what, .. } => what,
            Self::Rejected { reason } | Self::WontFix { reason } | Self::Unsettled { reason } => {
                reason
            }
        }
    }
}

/// One finding and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopFinding {
    /// Its number.
    pub id: FindingId,
    /// The round it was reported in.
    pub round: u32,
    /// What the reviewer reported.
    pub report: Report,
    /// The rejected finding it contests, if any.
    pub reopens: Option<FindingId>,
    /// Its verdict, once it has one.
    pub verdict: Option<LoopVerdict>,
}

/// Why a report is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// A field the reviewer left empty.
    Empty(&'static str),
    /// It contests a finding there is none of.
    Unknown(FindingId),
    /// It contests a finding that was not rejected.
    NotRejected(FindingId),
    /// The finding was contested once already, or is itself a contest: it
    /// is closed.
    Closed(FindingId),
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "{field} is required"),
            Self::Unknown(id) => write!(f, "there is no finding {id}"),
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

/// Why a verdict is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictError {
    /// There is no such finding.
    Unknown(FindingId),
    /// It was reported in another round.
    NotThisRound(FindingId),
    /// It has its verdict already.
    Given(FindingId),
    /// The verdict needs a reason, or a fixed one what changed.
    NoReason,
}

impl fmt::Display for VerdictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(id) => write!(f, "there is no finding {id}"),
            Self::NotThisRound(id) => write!(f, "{id} is not one of this round's findings"),
            Self::Given(id) => write!(f, "{id} has its verdict already"),
            Self::NoReason => f.write_str("a verdict needs its reason, or what you changed"),
        }
    }
}

/// Every finding of one loop, in the order reported.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Ledger {
    findings: Vec<LoopFinding>,
    round: u32,
}

impl Ledger {
    /// An empty ledger, before the first round.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts round `round`: reports and verdicts from now on belong to it.
    pub fn start_round(&mut self, round: u32) {
        self.round = round;
    }

    /// The round under way.
    #[must_use]
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Adds a finding of this round, or a contest of a rejected one.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError`] for an empty field, or a contest of a
    /// finding that is unknown, was not rejected, or is closed.
    pub fn report(
        &mut self,
        report: Report,
        reopens: Option<FindingId>,
    ) -> Result<FindingId, ReportError> {
        for (field, value) in [
            ("path", &report.path),
            ("claim", &report.claim),
            ("why", &report.why),
        ] {
            if value.trim().is_empty() {
                return Err(ReportError::Empty(field));
            }
        }
        if report.line == 0 {
            return Err(ReportError::Empty("line"));
        }
        if let Some(target) = reopens {
            let found = self.get(target).ok_or(ReportError::Unknown(target))?;
            if !matches!(found.verdict, Some(LoopVerdict::Rejected { .. })) {
                return Err(ReportError::NotRejected(target));
            }
            let contested = self.findings.iter().any(|f| f.reopens == Some(target));
            if found.reopens.is_some() || contested {
                return Err(ReportError::Closed(target));
            }
        }
        let id = FindingId(u32::try_from(self.findings.len()).unwrap_or(u32::MAX - 1) + 1);
        self.findings.push(LoopFinding {
            id,
            round: self.round,
            report,
            reopens,
            verdict: None,
        });
        Ok(id)
    }

    /// Gives a finding of this round its verdict. A fixed one holds once
    /// [`Ledger::pushed`] stamps it.
    ///
    /// # Errors
    ///
    /// Returns [`VerdictError`] for an unknown finding, one of another
    /// round, one that has its verdict, or a verdict without its text.
    pub fn give(&mut self, id: FindingId, verdict: LoopVerdict) -> Result<(), VerdictError> {
        if verdict.text().trim().is_empty() {
            return Err(VerdictError::NoReason);
        }
        let round = self.round;
        let finding = self
            .findings
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or(VerdictError::Unknown(id))?;
        if finding.round != round {
            return Err(VerdictError::NotThisRound(id));
        }
        if finding.verdict.is_some() {
            return Err(VerdictError::Given(id));
        }
        finding.verdict = Some(verdict);
        Ok(())
    }

    /// A finding by number.
    #[must_use]
    pub fn get(&self, id: FindingId) -> Option<&LoopFinding> {
        self.findings.iter().find(|f| f.id == id)
    }

    /// Every finding, in the order reported.
    pub fn iter(&self) -> impl Iterator<Item = &LoopFinding> + '_ {
        self.findings.iter()
    }

    /// The findings reported in `round`.
    pub fn reported_in(&self, round: u32) -> impl Iterator<Item = &LoopFinding> + '_ {
        self.findings.iter().filter(move |f| f.round == round)
    }

    /// This round's findings still waiting for a verdict.
    #[must_use]
    pub fn open(&self) -> Vec<&LoopFinding> {
        self.reported_in(self.round)
            .filter(|f| f.verdict.is_none())
            .collect()
    }

    /// Stamps every fixed finding that has no commit yet with `commit`, and
    /// returns them.
    pub fn pushed(&mut self, commit: &str) -> Vec<FindingId> {
        let mut stamped = Vec::new();
        for finding in &mut self.findings {
            if let Some(LoopVerdict::Fixed {
                commit: c @ None, ..
            }) = &mut finding.verdict
            {
                *c = Some(commit.to_owned());
                stamped.push(finding.id);
            }
        }
        stamped
    }

    /// Every fixed finding whose fix was not pushed becomes unsettled with
    /// `reason`; returns them.
    pub fn unpushed(&mut self, reason: &str) -> Vec<FindingId> {
        let mut settled = Vec::new();
        for finding in &mut self.findings {
            if matches!(
                finding.verdict,
                Some(LoopVerdict::Fixed { commit: None, .. })
            ) {
                finding.verdict = Some(LoopVerdict::Unsettled {
                    reason: reason.to_owned(),
                });
                settled.push(finding.id);
            }
        }
        settled
    }

    /// Every finding without a verdict becomes unsettled with `reason`;
    /// returns them. After [`Ledger::unpushed`] at the end of a run, every
    /// finding has its one final verdict.
    pub fn close_all(&mut self, reason: &str) -> Vec<FindingId> {
        let mut closed = Vec::new();
        for finding in &mut self.findings {
            if finding.verdict.is_none() {
                finding.verdict = Some(LoopVerdict::Unsettled {
                    reason: reason.to_owned(),
                });
                closed.push(finding.id);
            }
        }
        closed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn report(line: u32) -> Report {
        Report {
            path: "src/a.rs".into(),
            line,
            claim: "x is wrong".into(),
            why: "the caller divides by it".into(),
            fix: "make it 3".into(),
        }
    }

    fn rejected() -> LoopVerdict {
        LoopVerdict::Rejected {
            reason: "the caller checks for 0 at src/b.rs:9".into(),
        }
    }

    #[test]
    fn ids_count_up_and_read_back() {
        let mut ledger = Ledger::new();
        ledger.start_round(1);
        let first = ledger.report(report(2), None).unwrap();
        let second = ledger.report(report(3), None).unwrap();
        assert_eq!(
            (first.to_string(), second.to_string()),
            ("f1".into(), "f2".into())
        );
        assert_eq!(FindingId::parse("f2"), Some(second));
        assert_eq!(FindingId::parse("2"), Some(second));
        assert_eq!(FindingId::parse("f0"), None);
        let empty = Report {
            claim: " ".into(),
            ..report(2)
        };
        assert_eq!(ledger.report(empty, None), Err(ReportError::Empty("claim")));
    }

    #[test]
    fn a_rejection_is_contested_once_and_then_closed() {
        let mut ledger = Ledger::new();
        ledger.start_round(1);
        let fixed = ledger.report(report(2), None).unwrap();
        let first = ledger.report(report(3), None).unwrap();
        let what = LoopVerdict::Fixed {
            what: "x is 3".into(),
            commit: None,
        };
        ledger.give(fixed, what).unwrap();
        ledger.give(first, rejected()).unwrap();
        ledger.start_round(2);
        assert_eq!(
            ledger.report(report(2), Some(fixed)),
            Err(ReportError::NotRejected(fixed))
        );
        let contest = ledger.report(report(3), Some(first)).unwrap();
        assert_eq!(
            ledger.report(report(3), Some(first)),
            Err(ReportError::Closed(first)),
            "contested once already"
        );
        ledger.give(contest, rejected()).unwrap();
        ledger.start_round(3);
        assert_eq!(
            ledger.report(report(3), Some(contest)),
            Err(ReportError::Closed(contest)),
            "rejected twice"
        );
    }

    #[test]
    fn verdicts_only_for_this_rounds_open_findings() {
        let mut ledger = Ledger::new();
        ledger.start_round(1);
        let id = ledger.report(report(2), None).unwrap();
        assert_eq!(
            ledger.give(
                id,
                LoopVerdict::WontFix {
                    reason: String::new()
                }
            ),
            Err(VerdictError::NoReason)
        );
        ledger.give(id, rejected()).unwrap();
        assert_eq!(ledger.give(id, rejected()), Err(VerdictError::Given(id)));
        let later = ledger.report(report(4), None).unwrap();
        ledger.start_round(2);
        assert_eq!(
            ledger.give(later, rejected()),
            Err(VerdictError::NotThisRound(later))
        );
        let missing = FindingId::parse("f9").unwrap();
        assert_eq!(
            ledger.give(missing, rejected()),
            Err(VerdictError::Unknown(missing))
        );
    }

    #[test]
    fn a_fix_holds_once_pushed_and_every_finding_ends_with_one_verdict() {
        let mut ledger = Ledger::new();
        ledger.start_round(1);
        let pushed = ledger.report(report(2), None).unwrap();
        let fixed = LoopVerdict::Fixed {
            what: "x is 3".into(),
            commit: None,
        };
        ledger.give(pushed, fixed.clone()).unwrap();
        assert_eq!(ledger.pushed("abc123"), [pushed]);
        assert_eq!(ledger.pushed("def456"), [], "stamped once");

        ledger.start_round(2);
        let lost = ledger.report(report(3), None).unwrap();
        let silent = ledger.report(report(4), None).unwrap();
        ledger.give(lost, fixed).unwrap();
        assert_eq!(ledger.open().len(), 1);
        assert_eq!(ledger.unpushed("the push was refused"), [lost]);
        assert_eq!(ledger.close_all("the loop ended"), [silent]);
        let words: Vec<_> = ledger
            .iter()
            .map(|f| f.verdict.as_ref().unwrap().as_str())
            .collect();
        assert_eq!(words, ["fixed", "unsettled", "unsettled"]);
        assert!(matches!(
            &ledger.get(pushed).unwrap().verdict,
            Some(LoopVerdict::Fixed { commit: Some(c), .. }) if c == "abc123"
        ));
    }
}
