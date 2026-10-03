//! Code review (§3): triggers, lanes, outcomes and what the platform shows.

use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};

use crate::allowlist::{Platform, RepoRef};
use crate::identity::Requester;
use crate::marker::ModelId;
use crate::{GITHUB_HANDLE, GITLAB_HANDLE};

/// Why a commit sha was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("commit sha {0:?} must be 40 lowercase hex characters")]
pub struct CommitShaError(String);

/// A full git commit sha.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitSha(String);

impl CommitSha {
    /// Validates a full sha-1 commit id. Uppercase hex is accepted and lowered.
    ///
    /// # Errors
    ///
    /// Returns [`CommitShaError`] unless the value is exactly 40 hex digits.
    pub fn parse(value: &str) -> Result<Self, CommitShaError> {
        let value = value.trim();
        if value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(CommitShaError(value.to_owned()))
        }
    }

    /// The sha as lowercase hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first seven characters, as git shows them.
    #[must_use]
    pub fn short(&self) -> &str {
        self.0.get(..7).unwrap_or(&self.0)
    }
}

impl TryFrom<String> for CommitSha {
    type Error = CommitShaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<CommitSha> for String {
    fn from(sha: CommitSha) -> Self {
        sha.0
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One pull/merge request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReviewTarget {
    /// Repository.
    pub repo: RepoRef,
    /// Number (GitLab: iid).
    pub number: u64,
}

impl ReviewTarget {
    /// The platform.
    #[must_use]
    pub fn platform(&self) -> Platform {
        self.repo.platform()
    }
}

/// Henk's handle on a platform, as it appears in a mention.
#[must_use]
pub const fn handle(platform: Platform) -> &'static str {
    match platform {
        Platform::GitHub => GITHUB_HANDLE,
        Platform::GitLab => GITLAB_HANDLE,
    }
}

/// Whether a comment is a review request (§3.1): its whole text is
/// `@meneer-henk review` (GitLab: `@meneerhenk review`).
///
/// Surrounding whitespace is ignored and the handle is compared without
/// regard to case, as the platforms do. Anything more in the text makes it a
/// mention (§3.4), not a request.
#[must_use]
pub fn is_review_request(platform: Platform, body: &str) -> bool {
    let mut words = body.split_whitespace();
    let (Some(first), Some(second), None) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    first
        .strip_prefix('@')
        .is_some_and(|login| login.eq_ignore_ascii_case(handle(platform)))
        && second == "review"
}

/// Whether a comment mentions Henk on a platform (§3.4).
#[must_use]
pub fn mentions_henk(platform: Platform, body: &str) -> bool {
    let wanted = handle(platform);
    body.split(|c: char| {
        c.is_whitespace() || matches!(c, ',' | '.' | ':' | ';' | '!' | '?' | '(' | ')')
    })
    .filter_map(|word| word.strip_prefix('@'))
    .any(|login| login.eq_ignore_ascii_case(wanted))
}

/// What started a review (§3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTrigger {
    /// The pull/merge request was opened or reopened.
    Opened,
    /// New commits were pushed to an open pull/merge request.
    NewCommits,
    /// A person posted a review command on the pull/merge request.
    Command,
    /// A colleague asked in Discord.
    Discord(Requester),
    /// A request through the CLI or the HTTP API, on behalf of a Team Lead.
    Api(Requester),
}

/// One independent reviewer inside a review (§1.1, §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LaneName(String);

impl LaneName {
    /// Names a lane. The name is what the summary shows when the lane does
    /// not finish; it must not reveal the model (§3.2, open question 4).
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LaneName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A configured lane: a name the summary may show, and the model behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneSpec {
    /// Shown when the lane does not finish. Must not name the model (§3.2).
    pub name: LaneName,
    /// The configured model id this lane runs on.
    pub model: ModelId,
}

/// How one lane ended (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneOutcome {
    /// The lane read the change and posted what it found.
    Finished,
    /// The lane failed after retries or hung, and was dropped.
    Dropped,
}

/// One lane's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneResult {
    /// Which lane.
    pub lane: LaneName,
    /// How it ended.
    pub outcome: LaneOutcome,
}

/// The conclusion of the GitHub check "Meneer Henk" (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckConclusion {
    /// No issues found.
    Success,
    /// Issues found. Advisory; never blocks a merge (§8.2).
    Neutral,
    /// The review did not complete. Henk's failure, not the code's.
    Failure,
}

/// The GitLab commit status "Meneer Henk" (§3.3). It is always `success`
/// so it can never block a pipeline (§8.2); the count goes in the description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitStatus {
    /// The status description.
    pub description: String,
}

/// The outcome of one review of one commit (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// The commit that was reviewed.
    pub commit: CommitSha,
    /// Every lane and how it ended.
    pub lanes: Vec<LaneResult>,
    /// The number of Henk's distinct findings still open on the diff at this
    /// commit, including findings from earlier reviews whose line is still
    /// in the diff.
    pub open_findings: usize,
}

impl ReviewOutcome {
    /// Whether the review completed: at least one lane finished.
    #[must_use]
    pub fn completed(&self) -> bool {
        self.lanes
            .iter()
            .any(|lane| lane.outcome == LaneOutcome::Finished)
    }

    /// The lanes that were dropped, in order.
    pub fn dropped_lanes(&self) -> impl Iterator<Item = &LaneName> + '_ {
        self.lanes
            .iter()
            .filter(|lane| lane.outcome == LaneOutcome::Dropped)
            .map(|lane| &lane.lane)
    }

    /// The GitHub check conclusion.
    #[must_use]
    pub fn check_conclusion(&self) -> CheckConclusion {
        if !self.completed() {
            CheckConclusion::Failure
        } else if self.open_findings == 0 {
            CheckConclusion::Success
        } else {
            CheckConclusion::Neutral
        }
    }

    /// The GitLab commit status.
    #[must_use]
    pub fn commit_status(&self) -> CommitStatus {
        CommitStatus {
            description: self.headline(),
        }
    }

    /// The first line of the summary: the count, or that the review did not complete.
    #[must_use]
    pub fn headline(&self) -> String {
        if !self.completed() {
            return "Review did not complete.".to_owned();
        }
        match self.open_findings {
            0 => "No issues found.".to_owned(),
            1 => "1 issue found.".to_owned(),
            n => format!("{n} issues found."),
        }
    }

    /// The text of the summary comment or note (§3.3), without the run link
    /// and hidden marker, which the platform layer adds.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut text = self.headline();
        let dropped: Vec<&str> = self.dropped_lanes().map(LaneName::as_str).collect();
        match dropped.as_slice() {
            [] => {}
            [one] => {
                let _ = write!(text, " Lane {one} did not finish.");
            }
            many => {
                let _ = write!(text, " Lanes {} did not finish.", many.join(", "));
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn outcome(lanes: &[(&str, LaneOutcome)], open_findings: usize) -> ReviewOutcome {
        ReviewOutcome {
            commit: CommitSha::parse(SHA).unwrap_or_else(|e| panic!("{e}")),
            lanes: lanes
                .iter()
                .map(|(name, outcome)| LaneResult {
                    lane: LaneName::new(*name),
                    outcome: *outcome,
                })
                .collect(),
            open_findings,
        }
    }

    #[test]
    fn sha_is_validated_and_lowered() {
        let sha = CommitSha::parse(&SHA.to_ascii_uppercase()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(sha.as_str(), SHA);
        assert_eq!(sha.short(), "0123456");
        assert!(CommitSha::parse("abc").is_err());
        assert!(CommitSha::parse("g123456789abcdef0123456789abcdef01234567").is_err());
    }

    #[test]
    fn review_request_is_the_whole_text() {
        assert!(is_review_request(Platform::GitHub, "@meneer-henk review"));
        assert!(is_review_request(
            Platform::GitHub,
            "  @Meneer-Henk review\n"
        ));
        assert!(is_review_request(Platform::GitLab, "@meneerhenk review"));
        assert!(
            !is_review_request(Platform::GitLab, "@meneer-henk review"),
            "wrong handle"
        );
        assert!(!is_review_request(
            Platform::GitHub,
            "@meneer-henk review please"
        ));
        assert!(!is_review_request(Platform::GitHub, "@meneer-henk"));
        assert!(!is_review_request(Platform::GitHub, "review"));
    }

    #[test]
    fn mentions_are_detected_by_handle() {
        assert!(mentions_henk(
            Platform::GitHub,
            "Thanks @meneer-henk, good catch."
        ));
        assert!(mentions_henk(Platform::GitLab, "@meneerhenk?"));
        assert!(!mentions_henk(
            Platform::GitHub,
            "meneer-henk without an at sign"
        ));
        assert!(!mentions_henk(Platform::GitHub, "@meneer-henkie"));
    }

    #[test]
    fn no_findings_is_success() {
        let outcome = outcome(
            &[("a", LaneOutcome::Finished), ("b", LaneOutcome::Finished)],
            0,
        );
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Success);
        assert_eq!(outcome.summary(), "No issues found.");
    }

    #[test]
    fn findings_are_neutral_never_failure() {
        let outcome = outcome(&[("a", LaneOutcome::Finished)], 1);
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(outcome.summary(), "1 issue found.");
        assert_eq!(outcome.commit_status().description, "1 issue found.");
    }

    #[test]
    fn partial_success_is_success_and_names_dropped_lanes() {
        let outcome = outcome(
            &[
                ("a", LaneOutcome::Finished),
                ("b", LaneOutcome::Dropped),
                ("c", LaneOutcome::Dropped),
            ],
            3,
        );
        assert!(outcome.completed());
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(
            outcome.summary(),
            "3 issues found. Lanes b, c did not finish."
        );

        let one = outcome_one_dropped();
        assert_eq!(one.summary(), "No issues found. Lane b did not finish.");
    }

    fn outcome_one_dropped() -> ReviewOutcome {
        outcome(
            &[("a", LaneOutcome::Finished), ("b", LaneOutcome::Dropped)],
            0,
        )
    }

    #[test]
    fn no_lane_finished_is_henks_failure() {
        let outcome = outcome(&[("a", LaneOutcome::Dropped)], 0);
        assert!(!outcome.completed());
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Failure);
        assert_eq!(
            outcome.summary(),
            "Review did not complete. Lane a did not finish."
        );
    }
}
