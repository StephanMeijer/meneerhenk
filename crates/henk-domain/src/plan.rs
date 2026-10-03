//! Planning (§4): the plan section in an issue, the session log, and the
//! budget of tracker changes.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::allowlist::RepoRef;

/// One issue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IssueTarget {
    /// Repository.
    pub repo: RepoRef,
    /// Number (GitLab: iid).
    pub number: u64,
}

/// The heading of the folded plan section.
pub const PLAN_HEADING: &str = "Execution Plan by Meneer Henk";

/// The heading of the folded session log under the plan.
pub const SESSIONS_HEADING: &str = "Planning sessions";

/// Sub-issues one plan may create.
pub const SUB_ISSUE_CAP: u32 = 5;

/// Tracker changes one plan may make.
pub const DEFAULT_CHANGE_BUDGET: u32 = 20;

const START: &str = "<!-- meneer-henk:plan:start -->";
const END: &str = "<!-- meneer-henk:plan:end -->";
const SESSIONS_START: &str = "<!-- meneer-henk:sessions -->";

/// The plan and its session log, as stored in an issue description.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanSection {
    /// The plan in Markdown, without the folding wrapper.
    pub plan: String,
    /// One rendered line per planning session, oldest first.
    pub sessions: Vec<String>,
}

/// Renders the folded plan section.
#[must_use]
pub fn render_plan_section(section: &PlanSection) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{START}");
    let _ = writeln!(out, "<details>");
    let _ = writeln!(out, "<summary>{PLAN_HEADING}</summary>");
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", section.plan.trim());
    let _ = writeln!(out);
    let _ = writeln!(out, "{SESSIONS_START}");
    let _ = writeln!(out, "<details>");
    let _ = writeln!(out, "<summary>{SESSIONS_HEADING}</summary>");
    let _ = writeln!(out);
    for session in &section.sessions {
        let _ = writeln!(out, "- {}", session.trim());
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "</details>");
    let _ = writeln!(out, "</details>");
    let _ = write!(out, "{END}");
    out
}

/// Finds the plan section in an issue description.
#[must_use]
pub fn extract_plan(body: &str) -> Option<PlanSection> {
    let start = body.find(START)?;
    let after_start = start + START.len();
    let end = body.get(after_start..)?.find(END)? + after_start;
    let inner = body.get(after_start..end)?;

    let (plan_part, sessions_part) = match inner.find(SESSIONS_START) {
        Some(at) => (inner.get(..at)?, inner.get(at + SESSIONS_START.len()..)?),
        None => (inner, ""),
    };
    let plan = plan_part
        .lines()
        .filter(|line| {
            let t = line.trim();
            t != "<details>" && !t.starts_with("<summary>") && t != "</details>"
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    let sessions = sessions_part
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- ").map(str::to_owned))
        .collect();
    Some(PlanSection { plan, sessions })
}

/// The description without the plan section.
#[must_use]
pub fn body_without_plan(body: &str) -> String {
    let Some(start) = body.find(START) else {
        return body.to_owned();
    };
    let Some(end) = body
        .get(start..)
        .and_then(|rest| rest.find(END))
        .map(|e| start + e + END.len())
    else {
        return body.to_owned();
    };
    let mut out = String::new();
    out.push_str(body.get(..start).unwrap_or_default().trim_end());
    let tail = body.get(end..).unwrap_or_default().trim_start();
    if !tail.is_empty() {
        out.push_str("\n\n");
        out.push_str(tail);
    }
    out
}

/// The description with `section` replacing any existing plan section, or
/// appended at the end when there was none.
#[must_use]
pub fn with_plan_section(body: &str, section: &PlanSection) -> String {
    let rest = body_without_plan(body);
    let rendered = render_plan_section(section);
    if rest.trim().is_empty() {
        rendered
    } else {
        format!("{}\n\n{rendered}", rest.trim_end())
    }
}

/// One planning session for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// When, as text (RFC 3339).
    pub when: String,
    /// Which model.
    pub model: String,
    /// Who asked, as a stable id, when someone did.
    pub requester: Option<String>,
    /// What Henk changed, in words.
    pub changes: Vec<String>,
    /// Link to the run.
    pub run_link: String,
}

impl SessionEntry {
    /// One line for the log.
    #[must_use]
    pub fn render(&self) -> String {
        let who = self
            .requester
            .as_deref()
            .map_or(String::new(), |r| format!(", asked by {r}"));
        let changes = if self.changes.is_empty() {
            "no tracker changes".to_owned()
        } else {
            self.changes.join("; ")
        };
        format!(
            "{} by {}{who}: {changes}. [Run]({})",
            self.when, self.model, self.run_link
        )
    }
}

/// The budget ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the budget of {limit} tracker changes for this plan is spent")]
pub struct BudgetExhausted {
    /// The limit.
    pub limit: u32,
}

/// Tracker changes allowed per plan (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeBudget {
    limit: u32,
    used: u32,
}

impl ChangeBudget {
    /// A budget of `limit` changes.
    #[must_use]
    pub const fn new(limit: u32) -> Self {
        Self { limit, used: 0 }
    }

    /// Spends `cost` changes, or refuses without spending.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetExhausted`] when the cost does not fit.
    pub fn spend(&mut self, cost: u32) -> Result<(), BudgetExhausted> {
        if self.used.saturating_add(cost) > self.limit {
            return Err(BudgetExhausted { limit: self.limit });
        }
        self.used += cost;
        Ok(())
    }

    /// Changes made.
    #[must_use]
    pub const fn used(&self) -> u32 {
        self.used
    }

    /// Changes left.
    #[must_use]
    pub const fn remaining(&self) -> u32 {
        self.limit.saturating_sub(self.used)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn plan_section_round_trips_and_is_idempotent() {
        let body = "Original description.\n\nMore text.";
        let section = PlanSection {
            plan: "## Goal\n\nShip it.\n\n## Steps\n\n1. Do.".to_owned(),
            sessions: vec!["2026-10-03 by m: no tracker changes. [Run](https://h/r/1)".to_owned()],
        };
        let once = with_plan_section(body, &section);
        assert!(
            once.starts_with(
                "Original description.\n\nMore text.\n\n<!-- meneer-henk:plan:start -->"
            )
        );
        assert!(once.contains("<summary>Execution Plan by Meneer Henk</summary>"));
        assert_eq!(extract_plan(&once), Some(section.clone()));
        assert_eq!(body_without_plan(&once), body);

        let mut second = section.clone();
        second.plan = "## Goal\n\nShip it later.".to_owned();
        second
            .sessions
            .push("2026-10-04 by m: retitled. [Run](https://h/r/2)".to_owned());
        let twice = with_plan_section(&once, &second);
        assert_eq!(twice.matches("<!-- meneer-henk:plan:start -->").count(), 1);
        assert_eq!(extract_plan(&twice), Some(second));
        assert_eq!(body_without_plan(&twice), body);
    }

    #[test]
    fn plan_in_the_middle_keeps_the_text_after_it() {
        let section = PlanSection {
            plan: "P".into(),
            sessions: vec![],
        };
        let with = format!("Head\n\n{}\n\nTail", render_plan_section(&section));
        assert_eq!(body_without_plan(&with), "Head\n\nTail");
        assert_eq!(extract_plan(&with).unwrap().plan, "P");
        assert_eq!(extract_plan("no plan here"), None);
        assert_eq!(body_without_plan(""), "");
        assert!(with_plan_section("", &section).starts_with(START));
    }

    #[test]
    fn session_entries_render_one_line() {
        let entry = SessionEntry {
            when: "2026-10-03T10:00:00Z".into(),
            model: "claude".into(),
            requester: Some("523".into()),
            changes: vec!["added label bug".into(), "created sub-issue #4".into()],
            run_link: "https://h/runs/r-1".into(),
        };
        assert_eq!(
            entry.render(),
            "2026-10-03T10:00:00Z by claude, asked by 523: added label bug; created sub-issue #4. [Run](https://h/runs/r-1)"
        );
        assert!(crate::text::is_in_style(&entry.render()));
    }

    #[test]
    fn budget_refuses_without_spending() {
        let mut budget = ChangeBudget::new(3);
        assert!(budget.spend(2).is_ok());
        assert_eq!(budget.remaining(), 1);
        assert_eq!(budget.spend(2), Err(BudgetExhausted { limit: 3 }));
        assert_eq!(budget.used(), 2);
        assert!(budget.spend(1).is_ok());
        assert_eq!(budget.remaining(), 0);
    }
}
