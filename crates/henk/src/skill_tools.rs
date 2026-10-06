//! `load_skill` and the skills block of a system prompt (#72). An agent
//! with skills sees their names and descriptions in its system prompt and
//! loads a body with `load_skill`. The tool serves only that agent's own
//! skills, from memory; there is no path to name.

use std::collections::BTreeMap;
use std::sync::Arc;

use henk_agent::{Tool, ToolOutput, ToolSet, prompts};
use henk_domain::skill::{Skill, SkillName, render_catalogue};
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

/// The skills one agent may load.
pub type AgentSkills = BTreeMap<SkillName, Arc<Skill>>;

/// `load_skill`: the instructions of one of the agent's skills.
pub struct LoadSkill(pub Arc<AgentSkills>);

#[async_trait::async_trait]
impl Tool for LoadSkill {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("load_skill")
                .unwrap_or_else(|_| unreachable!("tool names here are constants")),
            description: "Loads the instructions of one of your skills, by its name from the skills list in your instructions. Load a skill before the work it is for.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "A skill name from your skills list"}
                },
                "required": ["name"]
            }),
        }
    }

    async fn call(&self, arguments: Value) -> ToolOutput {
        let asked = arguments
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let found = SkillName::parse(asked)
            .ok()
            .and_then(|name| self.0.get(&name));
        if let Some(skill) = found {
            return ToolOutput::ok(format!("Skill {}:\n\n{}", skill.name(), skill.body()));
        }
        let names: Vec<&str> = self.0.keys().map(SkillName::as_str).collect();
        ToolOutput::error(format!(
            "There is no skill {asked:?} for you. Your skills: {}.",
            names.join(", ")
        ))
    }

    /// A loaded skill is what the agent is following, so compaction keeps
    /// it as long as it can.
    fn keep_in_context(&self) -> bool {
        true
    }
}

/// Gives an agent its skills: the catalogue goes at the end of its system
/// prompt and `load_skill` into its tools. Without skills both are left as
/// they are, so a prompt without skills is the same as before (#72).
pub fn equip(system: &mut String, tools: &mut ToolSet, skills: AgentSkills) {
    if skills.is_empty() {
        return;
    }
    let catalogue = render_catalogue(skills.values().map(AsRef::as_ref));
    system.push_str("\n\n");
    system.push_str(&prompts::render(
        prompts::SKILLS,
        &[("catalogue", &catalogue)],
    ));
    tools.add(LoadSkill(Arc::new(skills)));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::time::Duration;

    use henk_agent::{Agent, AgentConfig, StopCause};
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{
        Block, ChatMessage, Completion, Role, StopReason, ToolArguments, ToolCall, Usage,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn skill(name: &str, body: &str) -> (SkillName, Arc<Skill>) {
        let text = format!("---\nname: {name}\ndescription: Use for {name}.\n---\n{body}");
        let skill = Skill::parse(name, &text).unwrap();
        (skill.name().clone(), Arc::new(skill))
    }

    fn skills() -> AgentSkills {
        [skill(
            "sql-migrations",
            "Every migration can be rolled back.",
        )]
        .into_iter()
        .collect()
    }

    #[tokio::test]
    async fn load_skill_serves_only_the_agents_own_skills() {
        let tool = LoadSkill(Arc::new(skills()));
        let loaded = tool.call(json!({"name": "sql-migrations"})).await;
        assert!(!loaded.is_error);
        assert_eq!(
            loaded.content,
            "Skill sql-migrations:\n\nEvery migration can be rolled back."
        );
        for asked in [
            json!({"name": "rust-errors"}),
            json!({"name": "../etc/passwd"}),
            json!({}),
        ] {
            let refused = tool.call(asked).await;
            assert!(refused.is_error);
            assert!(refused.content.contains("Your skills: sql-migrations."));
        }
        assert!(tool.keep_in_context());
    }

    #[test]
    fn equip_adds_the_catalogue_and_the_tool_only_when_there_are_skills() {
        let mut system = "base".to_owned();
        let mut tools = ToolSet::new();
        equip(&mut system, &mut tools, AgentSkills::new());
        assert_eq!(system, "base");
        assert_eq!(tools.names().count(), 0);

        equip(&mut system, &mut tools, skills());
        assert!(system.starts_with("base\n\n"));
        assert!(system.contains("- sql-migrations: Use for sql-migrations."));
        assert!(!system.contains("{{catalogue}}"));
        assert!(henk_domain::text::is_in_style(&system));
        assert_eq!(tools.names().collect::<Vec<_>>(), ["load_skill"]);
    }

    /// A model that calls `load_skill` gets the body back as the result.
    #[tokio::test]
    async fn an_agent_loads_a_skill() {
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 1,
        };
        let call = Completion {
            message: ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall(ToolCall {
                    id: "c1".to_owned(),
                    name: "load_skill".to_owned(),
                    arguments: ToolArguments::Parsed(json!({"name": "sql-migrations"})),
                })],
            },
            stop: StopReason::ToolUse,
            usage,
        };
        let done = Completion {
            message: ChatMessage::assistant("done"),
            stop: StopReason::EndTurn,
            usage,
        };
        let model = Arc::new(ScriptedClient::new("scripted", [Ok(call), Ok(done)]));
        let mut system = "base".to_owned();
        let mut tools = ToolSet::new();
        equip(&mut system, &mut tools, skills());
        let config = AgentConfig {
            max_turns: 3,
            timeout: Duration::from_secs(5),
            ..AgentConfig::default()
        };
        let outcome = Agent::new(model, tools, &system, config)
            .run(vec![ChatMessage::user("go")], CancellationToken::new())
            .await;
        assert!(matches!(outcome.stop, StopCause::EndTurn));
        let result = outcome
            .messages
            .iter()
            .flat_map(|m| &m.blocks)
            .find_map(|b| match b {
                Block::ToolResult(r) => Some(r),
                _ => None,
            })
            .unwrap();
        assert!(!result.is_error);
        assert!(
            result
                .content
                .contains("Every migration can be rolled back.")
        );
    }
}
