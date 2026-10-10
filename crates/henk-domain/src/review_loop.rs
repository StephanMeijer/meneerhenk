//! The findings of a reviewer↔fixer loop (#285): what the reviewer reports,
//! and the one verdict the fixer, or the loop's end, gives each.
//!
//! The reviewer may contest a rejection once, by reporting the finding
//! again with new evidence; a finding rejected twice is closed. A `fixed`
//! verdict holds once Henk pushed the commit; when nothing was pushed it
//! becomes `unsettled`, as does every finding still open when the run ends.

use std::collections::BTreeSet;
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

impl LoopFinding {
    /// The finding and its verdict, in one line for the reviewer: `f1 at
    /// src/a.rs:2 (x must be 3): fixed in 4e735d378f06: x is 3 now.`
    #[must_use]
    pub fn verdict_line(&self) -> String {
        let report = &self.report;
        let fate = match &self.verdict {
            None => "no verdict".to_owned(),
            Some(LoopVerdict::Fixed { what, commit }) => match commit {
                Some(sha) => format!("fixed in {}: {what}", sha.get(..12).unwrap_or(sha)),
                None => format!("fixed: {what}"),
            },
            Some(verdict) => format!("{}: {}", verdict.as_str().replace('_', " "), verdict.text()),
        };
        format!(
            "{} at {}:{} ({}): {fate}",
            self.id, report.path, report.line, report.claim
        )
    }
}

/// Lines apart a repeat may be: a fix moves the code around it.
const REPEAT_LINES: u32 = 3;

/// The share of claim words, in percent, a repeat has in common with what
/// it repeats.
const REPEAT_OVERLAP_PERCENT: usize = 60;

/// A claim's words, lowercased, without the short ones.
fn claim_words(claim: &str) -> BTreeSet<String> {
    claim
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

/// Whether two claims share most of their words (Jaccard).
fn similar_claims(a: &str, b: &str) -> bool {
    let (a, b) = (claim_words(a), claim_words(b));
    let union = a.union(&b).count();
    union > 0 && a.intersection(&b).count() * 100 >= union * REPEAT_OVERLAP_PERCENT
}

/// Why a loop stopped (#286), as the run records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopStop {
    /// The reviewer found nothing new.
    Converged,
    /// The rounds ran out.
    MaxRounds(u32),
    /// The reviewer reported again what the fixer fixed: they are going in
    /// circles.
    RepeatingFinding {
        /// The new report.
        finding: FindingId,
        /// The fixed finding it repeats.
        of: FindingId,
    },
    /// The checks fail after the fixer's changes; nothing was pushed.
    BuildFailing(String),
    /// The run's time limit, or a session's in a round.
    Timeout(String),
    /// A session broke off: a model error, a refusal, a turn limit.
    SessionFailed(String),
    /// The reviewer changed files in the workspace.
    WorkspaceChanged,
    /// The fixer's changes could not be pushed.
    PushRefused(String),
    /// Someone or something cancelled the run.
    Cancelled,
}

impl LoopStop {
    /// The stored word.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Converged => "converged",
            Self::MaxRounds(_) => "max_rounds",
            Self::RepeatingFinding { .. } => "repeating_finding",
            Self::BuildFailing(_) => "build_failing",
            Self::Timeout(_) => "timeout",
            Self::SessionFailed(_) => "session_failed",
            Self::WorkspaceChanged => "workspace_changed",
            Self::PushRefused(_) => "push_refused",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the loop ended as it should: it converged, or a limit set
    /// for it ended it. Anything else is a failure of the loop.
    #[must_use]
    pub fn ended_well(&self) -> bool {
        matches!(
            self,
            Self::Converged | Self::MaxRounds(_) | Self::RepeatingFinding { .. }
        )
    }

    /// Whether the loop stopped with the reviewer's last findings open: at
    /// `max_rounds` the fixer's answer to them was never reviewed, so the
    /// review is no pass even with no finding left unsettled.
    #[must_use]
    pub fn left_findings_open(&self) -> bool {
        matches!(self, Self::MaxRounds(_))
    }
}

impl fmt::Display for LoopStop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Converged => f.write_str("converged, the reviewer found nothing more"),
            Self::MaxRounds(rounds) => write!(f, "it reached max_rounds ({rounds})"),
            Self::RepeatingFinding { finding, of } => write!(
                f,
                "the reviewer reported {of} again as {finding} after it was fixed"
            ),
            Self::BuildFailing(why)
            | Self::Timeout(why)
            | Self::SessionFailed(why)
            | Self::PushRefused(why) => f.write_str(why),
            Self::WorkspaceChanged => {
                f.write_str("the reviewer changed the workspace; nothing was pushed")
            }
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
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

    /// The fixed and pushed finding `report` repeats, if any: the same file,
    /// a line at most three apart and most claim words in common.
    #[must_use]
    pub fn repeat_of(&self, report: &Report) -> Option<FindingId> {
        self.findings
            .iter()
            .filter(|f| {
                matches!(
                    f.verdict,
                    Some(LoopVerdict::Fixed {
                        commit: Some(_),
                        ..
                    })
                )
            })
            .find(|f| {
                f.report.path == report.path
                    && f.report.line.abs_diff(report.line) <= REPEAT_LINES
                    && similar_claims(&f.report.claim, &report.claim)
            })
            .map(|f| f.id)
    }

    /// The findings of the rounds before `round`, one line each with its
    /// verdict: what a conversation keeps of the rounds it no longer holds.
    #[must_use]
    pub fn summary(&self, round: u32) -> String {
        let lines: Vec<String> = self
            .findings
            .iter()
            .filter(|f| f.round < round)
            .map(LoopFinding::verdict_line)
            .collect();
        if lines.is_empty() {
            "No findings so far.".to_owned()
        } else {
            lines.join("\n")
        }
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

    fn pushed_fix(ledger: &mut Ledger, line: u32, claim: &str) -> FindingId {
        let id = ledger
            .report(
                Report {
                    claim: claim.into(),
                    ..report(line)
                },
                None,
            )
            .unwrap();
        let fixed = LoopVerdict::Fixed {
            what: "done".into(),
            commit: None,
        };
        ledger.give(id, fixed).unwrap();
        ledger.pushed("abc123");
        id
    }

    #[test]
    fn a_repeat_is_a_fixed_finding_reported_again_nearby() {
        let mut ledger = Ledger::new();
        ledger.start_round(1);
        let fixed = pushed_fix(&mut ledger, 10, "x must be 3, the caller divides by it");
        let again = |line: u32, path: &str, claim: &str| Report {
            path: path.into(),
            claim: claim.into(),
            ..report(line)
        };
        let reworded = again(12, "src/a.rs", "X must be 3: the caller divides by it!");
        assert_eq!(ledger.repeat_of(&reworded), Some(fixed));
        assert_eq!(
            ledger.repeat_of(&again(
                14,
                "src/a.rs",
                "x must be 3, the caller divides by it"
            )),
            None,
            "too far"
        );
        assert_eq!(
            ledger.repeat_of(&again(
                10,
                "src/b.rs",
                "x must be 3, the caller divides by it"
            )),
            None,
            "another file"
        );
        assert_eq!(
            ledger.repeat_of(&again(
                10,
                "src/a.rs",
                "the loop never ends on an empty list"
            )),
            None,
            "another claim"
        );

        // Only a fix that was pushed counts.
        let unpushed = ledger.report(report(30), None).unwrap();
        ledger.give(unpushed, rejected()).unwrap();
        assert_eq!(ledger.repeat_of(&report(30)), None);
        assert!(ledger.summary(2).contains("f1 at src/a.rs:10"));
        assert_eq!(Ledger::new().summary(1), "No findings so far.");
    }

    #[test]
    fn stops_have_their_words_and_say_whether_the_loop_ended_well() {
        let id = FindingId::parse("f2").unwrap();
        let of = FindingId::parse("f1").unwrap();
        let repeat = LoopStop::RepeatingFinding { finding: id, of };
        assert_eq!(repeat.as_str(), "repeating_finding");
        assert_eq!(
            repeat.to_string(),
            "the reviewer reported f1 again as f2 after it was fixed"
        );
        assert!(repeat.ended_well() && LoopStop::Converged.ended_well());
        assert!(!LoopStop::Timeout("time".into()).ended_well());
        assert!(LoopStop::MaxRounds(3).left_findings_open());
        assert!(!LoopStop::Converged.left_findings_open());
        assert!(!repeat.left_findings_open(), "the repeat is unsettled");
        assert_eq!(
            LoopStop::MaxRounds(50).to_string(),
            "it reached max_rounds (50)"
        );
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
