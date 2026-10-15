//! Roles (#171, #321): the agents Henk runs, and the tools each may ever
//! have. A role's ceiling is a pure rule (§8.4): only the executor edits,
//! the handoff tools belong to the pair that uses them, the tracker and
//! web tools are the planner's, and the platform's read tools are the
//! planner's and the reviewer's, who reviews plans as well as code (#328).
//! Configuration may narrow a role's tools, never widen them.

use std::fmt;

/// One of the agents Henk runs. A session in a run has one role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// Turns an issue into a plan and sub-issues.
    Planner,
    /// Reviews a pull request and hands its findings to the executor, or a
    /// drafted plan and hands its points to the planner (#328).
    Reviewer,
    /// Changes the code: fixes what the reviewer found, or takes an issue
    /// to a pull request (#329).
    Executor,
}

/// The code tools every role reads with.
const CODE: &[&str] = &["list_files", "read_file", "search"];
/// Running commands in the role's own copy.
const BASH: &str = "bash";
/// Fetching a web page; its text is data (§8.3).
const WEB_FETCH: &str = "web_fetch";
/// Loading one of the skills the role lists.
const LOAD_SKILL: &str = "load_skill";

/// What only the reviewer has: the diff, the pull request's CI (#420) and
/// its side of both handoffs.
const REVIEWER: &[&str] = &[
    "list_changed_files",
    "get_file_diff",
    "list_ci_jobs",
    "get_ci_job_log",
    "get_ci_job_annotations",
    "report_finding",
    "raise_point",
    "finish_round",
];
/// What only the executor has: editing, its side of the handoff, and
/// proposing the pull request for an issue (#329). It runs the
/// repository's checks with `bash` (#396).
const EXECUTOR: &[&str] = &[
    "edit_file",
    "write_file",
    "list_findings",
    "give_verdict",
    "propose_commits",
    "propose_pull_request",
];
/// What only the planner has: the tracker tools, which draft until the
/// review is done, and its side of the plan's handoff.
const PLANNER: &[&str] = &[
    "set_title",
    "set_description",
    "add_labels",
    "set_issue_type",
    "set_fields",
    "link_issue",
    "create_sub_issue",
    "ask_questions",
    "write_plan",
    "withdraw_change",
    "list_points",
    "answer_point",
];

impl Role {
    /// Every role.
    pub const ALL: [Self; 3] = [Self::Planner, Self::Reviewer, Self::Executor];

    /// The role's name, as configuration and the run record spell it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Reviewer => "reviewer",
            Self::Executor => "executor",
        }
    }

    /// The role a name stands for.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.name() == name)
    }

    /// The tools only this role has.
    fn own(self) -> &'static [&'static str] {
        match self {
            Self::Planner => PLANNER,
            Self::Reviewer => REVIEWER,
            Self::Executor => EXECUTOR,
        }
    }

    /// Whether the role may use an MCP server's tools, which the scope
    /// guard filters (§8.5): the planner, and the reviewer of its plan.
    fn has_platform_tools(self) -> bool {
        matches!(self, Self::Planner | Self::Reviewer)
    }

    /// The built-in tools the role may ever have, in a fixed order. Only
    /// the planner's session is built with `web_fetch`, so only the
    /// planner may have it: a grant can leave a tool out, never add one.
    #[must_use]
    pub fn ceiling(self) -> Vec<&'static str> {
        let mut tools = CODE.to_vec();
        tools.push(BASH);
        if self == Self::Planner {
            tools.push(WEB_FETCH);
        }
        tools.push(LOAD_SKILL);
        tools.extend(self.own());
        tools
    }

    /// The built-in tools the role gets when its configuration names none:
    /// its ceiling.
    #[must_use]
    pub fn default_tools(self) -> Vec<String> {
        self.ceiling().into_iter().map(str::to_owned).collect()
    }

    /// Whether the role may have the tool `name`: one of its ceiling, or
    /// an MCP server's (`<server>__<tool>`) for a role with platform tools.
    #[must_use]
    pub fn allows(self, name: &str) -> bool {
        if is_platform_tool(name) {
            return self.has_platform_tools();
        }
        self.ceiling().contains(&name)
    }

    /// Whether `name` is a tool granted in `granted`. A platform tool is
    /// granted with its role; the scope guard decides which of them run.
    #[must_use]
    pub fn grants(self, granted: &[String], name: &str) -> bool {
        if is_platform_tool(name) {
            return self.has_platform_tools();
        }
        granted.iter().any(|tool| tool == name)
    }

    /// The handoff tools the role needs to plan (#328): without them the
    /// reviewer cannot raise a point or end its round, and the planner
    /// cannot write the plan or answer a point. The executor has none.
    #[must_use]
    pub fn plan_handoff(self) -> &'static [&'static str] {
        match self {
            Self::Planner => &["write_plan", "answer_point"],
            Self::Reviewer => &["raise_point", "finish_round"],
            Self::Executor => &[],
        }
    }

    /// The first plan handoff tool of the role's that `granted` leaves out.
    #[must_use]
    pub fn missing_plan_handoff(self, granted: &[String]) -> Option<&'static str> {
        self.plan_handoff()
            .iter()
            .copied()
            .find(|tool| !granted.iter().any(|g| g == tool))
    }

    /// The tools the role gets from its configured list: the defaults
    /// when there is none.
    ///
    /// # Errors
    ///
    /// [`CeilingError`] for a tool no role has, or one above this role's
    /// ceiling.
    pub fn narrow(self, configured: Option<&[String]>) -> Result<Vec<String>, CeilingError> {
        let Some(configured) = configured else {
            return Ok(self.default_tools());
        };
        for tool in configured {
            if !Self::ALL
                .iter()
                .any(|role| role.ceiling().contains(&tool.as_str()))
            {
                return Err(CeilingError::Unknown {
                    role: self,
                    tool: tool.clone(),
                });
            }
            if !self.ceiling().contains(&tool.as_str()) {
                return Err(CeilingError::AboveCeiling {
                    role: self,
                    tool: tool.clone(),
                });
            }
        }
        Ok(configured.to_vec())
    }

    /// The handoff tools the role needs in the review loop: without them
    /// the reviewer cannot hand over a finding or end its round, and the
    /// executor cannot read the findings or give a verdict. The planner
    /// has none.
    #[must_use]
    pub fn handoff(self) -> &'static [&'static str] {
        match self {
            Self::Planner => &[],
            Self::Reviewer => &["report_finding", "finish_round"],
            Self::Executor => &["list_findings", "give_verdict"],
        }
    }

    /// The first handoff tool of the role's that `granted` leaves out.
    #[must_use]
    pub fn missing_handoff(self, granted: &[String]) -> Option<&'static str> {
        self.handoff()
            .iter()
            .copied()
            .find(|tool| !granted.iter().any(|g| g == tool))
    }

    /// Whether these granted tools reach the web.
    #[must_use]
    pub fn reaches_web(granted: &[String]) -> bool {
        granted.iter().any(|tool| tool == WEB_FETCH)
    }

    /// Whether these granted tools can load a skill. A role that lists
    /// skills needs `load_skill`, or its prompt would name skills it
    /// cannot load.
    #[must_use]
    pub fn loads_skills(granted: &[String]) -> bool {
        granted.iter().any(|tool| tool == LOAD_SKILL)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An MCP server's tool: `<server>__<tool>`. Henk's own tools never have
/// a `__`.
fn is_platform_tool(name: &str) -> bool {
    name.contains("__")
}

/// Why a configured tool list was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CeilingError {
    /// No role has a tool by this name.
    Unknown {
        /// The role it was configured for.
        role: Role,
        /// The name.
        tool: String,
    },
    /// The tool is above this role's ceiling.
    AboveCeiling {
        /// The role it was configured for.
        role: Role,
        /// The tool.
        tool: String,
    },
}

/// The executor's tool that ran Henk's configured checks, until the
/// executor ran the repository's own with `bash` (#396).
const REMOVED_RUN_CHECKS: &str = "run_checks";

impl fmt::Display for CeilingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { role, tool } if tool == REMOVED_RUN_CHECKS => write!(
                f,
                "[roles.{role}] tools: run_checks was removed: the executor runs the repository's own checks with bash; the README's \"Migrating to repository checks\" says how"
            ),
            Self::Unknown { role, tool } => {
                write!(f, "[roles.{role}] tools: {tool} is not a tool Henk has")
            }
            Self::AboveCeiling { role, tool } => {
                write!(f, "[roles.{role}] tools: the {role} may not have {tool}")
            }
        }
    }
}

impl std::error::Error for CeilingError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_planning_roles_keep_their_side_of_the_plan_handoff() {
        for role in Role::ALL {
            assert_eq!(
                role.missing_plan_handoff(&role.default_tools()),
                None,
                "{role}"
            );
            assert!(
                role.plan_handoff().iter().all(|tool| role.allows(tool)),
                "{role}"
            );
        }
        assert_eq!(
            Role::Reviewer.missing_plan_handoff(&tools(&[
                "read_file",
                "report_finding",
                "finish_round"
            ])),
            Some("raise_point")
        );
        assert_eq!(
            Role::Planner.missing_plan_handoff(&tools(&["read_file", "write_plan"])),
            Some("answer_point")
        );
        assert_eq!(Role::Executor.missing_plan_handoff(&[]), None);
    }

    #[test]
    fn a_role_reads_back_from_its_name() {
        for role in Role::ALL {
            assert_eq!(Role::parse(role.name()), Some(role));
        }
        assert_eq!(Role::parse("fixer"), None);
        assert_eq!(Role::parse("lane"), None);
    }

    #[test]
    fn only_the_executor_edits_and_only_the_planner_writes_to_the_tracker() {
        for role in Role::ALL {
            assert_eq!(role.allows("edit_file"), role == Role::Executor, "{role}");
            assert!(!role.allows("run_checks"), "{role}: removed in #396");
            assert_eq!(
                role.allows("create_sub_issue"),
                role == Role::Planner,
                "{role}"
            );
            assert_eq!(
                role.allows("github__get_issue"),
                role != Role::Executor,
                "{role}"
            );
            assert!(role.allows("read_file") && role.allows("bash"), "{role}");
        }
    }

    #[test]
    fn the_handoff_tools_belong_to_their_side() {
        assert!(Role::Reviewer.allows("report_finding"));
        assert!(Role::Reviewer.allows("finish_round"));
        assert!(!Role::Executor.allows("report_finding"));
        assert!(Role::Executor.allows("give_verdict"));
        assert!(Role::Executor.allows("list_findings"));
        assert!(!Role::Reviewer.allows("give_verdict"));
        assert!(!Role::Planner.allows("report_finding"));
        assert!(Role::Reviewer.allows("raise_point"));
        assert!(!Role::Planner.allows("raise_point"));
        assert!(Role::Planner.allows("answer_point"));
        assert!(Role::Planner.allows("withdraw_change"));
        assert!(!Role::Reviewer.allows("answer_point"));
    }

    #[test]
    fn by_default_only_the_planner_reaches_the_web() {
        for role in Role::ALL {
            let defaults = role.default_tools();
            assert_eq!(
                Role::reaches_web(&defaults),
                role == Role::Planner,
                "{role}"
            );
            assert!(defaults.iter().all(|tool| role.allows(tool)), "{role}");
        }
        assert!(Role::Planner.allows("web_fetch"));
        assert!(!Role::Reviewer.allows("web_fetch"), "its session has none");
        assert!(!Role::Executor.allows("web_fetch"), "its session has none");
        assert_eq!(
            Role::Reviewer
                .narrow(Some(&tools(&["read_file", "web_fetch"])))
                .unwrap_err()
                .to_string(),
            "[roles.reviewer] tools: the reviewer may not have web_fetch"
        );
    }

    #[test]
    fn the_loop_roles_keep_their_side_of_the_handoff() {
        for role in Role::ALL {
            assert_eq!(role.missing_handoff(&role.default_tools()), None, "{role}");
            assert!(
                role.handoff().iter().all(|tool| role.allows(tool)),
                "{role}"
            );
        }
        assert_eq!(
            Role::Reviewer.missing_handoff(&tools(&["read_file", "report_finding"])),
            Some("finish_round")
        );
        assert_eq!(
            Role::Executor.missing_handoff(&tools(&["edit_file", "give_verdict"])),
            Some("list_findings")
        );
        assert_eq!(Role::Planner.missing_handoff(&[]), None);
    }

    #[test]
    fn skills_are_loaded_with_load_skill() {
        for role in Role::ALL {
            assert!(Role::loads_skills(&role.default_tools()), "{role}");
        }
        assert!(!Role::loads_skills(&tools(&["read_file"])));
        assert!(Role::loads_skills(&tools(&["read_file", "load_skill"])));
    }

    #[test]
    fn a_configured_list_may_only_narrow() {
        let narrowed = Role::Reviewer
            .narrow(Some(&tools(&["read_file", "report_finding"])))
            .unwrap();
        assert_eq!(narrowed, tools(&["read_file", "report_finding"]));
        assert!(
            !narrowed.contains(&"bash".to_owned()),
            "bash can be left out"
        );
        assert_eq!(
            Role::Executor.narrow(None).unwrap(),
            Role::Executor.default_tools()
        );

        assert_eq!(
            Role::Reviewer.narrow(Some(&tools(&["edit_file"]))),
            Err(CeilingError::AboveCeiling {
                role: Role::Reviewer,
                tool: "edit_file".into()
            })
        );
        assert_eq!(
            Role::Executor
                .narrow(Some(&tools(&["create_sub_issue"])))
                .unwrap_err()
                .to_string(),
            "[roles.executor] tools: the executor may not have create_sub_issue"
        );
        assert_eq!(
            Role::Planner
                .narrow(Some(&tools(&["teleport"])))
                .unwrap_err()
                .to_string(),
            "[roles.planner] tools: teleport is not a tool Henk has"
        );
        let removed = Role::Executor
            .narrow(Some(&tools(&["edit_file", "run_checks"])))
            .unwrap_err()
            .to_string();
        assert!(
            removed.starts_with("[roles.executor] tools: run_checks was removed"),
            "{removed}"
        );
        assert!(
            removed.contains("\"Migrating to repository checks\""),
            "{removed}"
        );
        assert!(crate::text::is_in_style(&removed), "{removed}");
    }

    #[test]
    fn the_reviewer_reads_ci_by_default_and_may_leave_it_out() {
        for tool in ["list_ci_jobs", "get_ci_job_log", "get_ci_job_annotations"] {
            assert!(
                Role::Reviewer.default_tools().contains(&tool.to_owned()),
                "{tool}"
            );
            assert!(
                !Role::Executor.allows(tool),
                "{tool}: not the executor's yet"
            );
            assert!(!Role::Planner.allows(tool), "{tool}");
        }
        let without = Role::Reviewer
            .narrow(Some(&tools(&[
                "get_file_diff",
                "report_finding",
                "finish_round",
            ])))
            .unwrap();
        assert!(!without.contains(&"list_ci_jobs".to_owned()));
    }

    #[test]
    fn a_platform_tool_comes_with_the_role_not_the_list() {
        let none: Vec<String> = Vec::new();
        assert!(Role::Planner.grants(&none, "gitlab__get_issue"));
        assert!(Role::Reviewer.grants(&none, "github__get_file"));
        assert!(!Role::Executor.grants(&Role::Executor.default_tools(), "github__get_file"));
        assert!(!Role::Planner.grants(&none, "read_file"));
    }
}
