//! A role's tools (#171): the set a session is built with, cut to what its
//! role grants. Every role session goes through [`grant`] once, when it is
//! built, so its tools stay the same for as long as it runs and a resumed
//! turn keeps the cached prompt prefix (#321).

use henk_agent::ToolSet;
use henk_domain::role::Role;

use crate::skill_tools::{AgentSkills, equip};

/// `set`, keeping only the tools `role` grants in `granted`. A tool above
/// the role's ceiling is a bug in the code that built the set: tests and
/// debug builds stop on it, release builds leave it out.
pub fn grant(role: Role, mut set: ToolSet, granted: &[String]) -> ToolSet {
    for name in set.names() {
        debug_assert!(
            role.allows(name),
            "the {role} was built with {name}, above its ceiling"
        );
    }
    set.retain(|name| role.allows(name) && role.grants(granted, name));
    set
}

/// `set` with the role's `skills` added (their catalogue goes in `system`),
/// then cut to what `role` grants in `granted`.
pub fn equip_and_grant(
    role: Role,
    system: &mut String,
    mut set: ToolSet,
    skills: AgentSkills,
    granted: &[String],
) -> ToolSet {
    equip(system, &mut set, skills);
    grant(role, set, granted)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use henk_agent::{Tool, ToolOutput};
    use henk_llm::{ToolDef, ToolName};
    use serde_json::{Value, json};

    use super::*;

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl Tool for Named {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: ToolName::parse(self.0).unwrap(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
            }
        }

        async fn call(&self, _: Value) -> ToolOutput {
            ToolOutput::ok("")
        }
    }

    fn set(names: &[&'static str]) -> ToolSet {
        let mut set = ToolSet::new();
        for name in names {
            set.add_arc(Arc::new(Named(name)));
        }
        set
    }

    #[test]
    fn a_tool_left_out_of_the_grant_is_not_given() {
        let granted: Vec<String> = Role::Reviewer
            .default_tools()
            .into_iter()
            .filter(|tool| tool != "bash")
            .collect();
        let tools = grant(
            Role::Reviewer,
            set(&["bash", "read_file", "report_finding"]),
            &granted,
        );
        assert_eq!(
            tools.names().collect::<Vec<_>>(),
            ["read_file", "report_finding"]
        );
    }

    #[test]
    fn the_planner_keeps_its_platform_tools() {
        let tools = grant(
            Role::Planner,
            set(&["github__get_issue", "write_plan"]),
            &Role::Planner.default_tools(),
        );
        assert_eq!(tools.len(), 2);
    }

    #[test]
    #[should_panic(expected = "above its ceiling")]
    fn a_set_built_above_the_ceiling_is_a_bug() {
        let _ = grant(
            Role::Reviewer,
            set(&["edit_file"]),
            &Role::Reviewer.default_tools(),
        );
    }
}
