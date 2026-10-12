//! Prompt templates, compiled into the binary.

/// Henk's persona (spec §7). Prepended to every system prompt.
pub const PERSONA: &str = include_str!("../prompts/persona.md");

/// Appended to the executor's instructions when its workspace lets the
/// model run commands of its own (#172).
pub const EXECUTOR_BASH: &str = include_str!("../prompts/executor_bash.md");

/// Appended to the planner's instructions when it has a workspace at the
/// default branch (#172). `{{branch}}` and `{{commit}}` say which;
/// `{{tools}}` is a sentence on the code tools the planner was granted.
pub const PLAN_WORKSPACE: &str = include_str!("../prompts/plan_workspace.md");

/// Instructions for the planner (§4).
pub const PLANNER: &str = include_str!("../prompts/planner.md");

/// Instructions for the reviewer of a review loop (#284). `{{ref}}` and
/// `{{repo}}` say which pull request.
pub const LOOP_REVIEWER: &str = include_str!("../prompts/loop_reviewer.md");

/// Instructions for the executor of a review loop (#284). `{{ref}}`,
/// `{{repo}}` and `{{max_files}}` are filled in.
pub const LOOP_EXECUTOR: &str = include_str!("../prompts/loop_executor.md");

/// The skills block appended to an agent's system prompt when it has
/// skills. `{{catalogue}}` lists them.
pub const SKILLS: &str = include_str!("../prompts/skills.md");

/// Fixed greetings for mentions (§3.4), one per line. No model is involved,
/// so a mention cannot inject anything.
pub const GREETINGS: &str = include_str!("../prompts/greetings.txt");

/// Picks a greeting by a stable key, so the same comment gets the same line.
#[must_use]
pub fn greeting(key: u64) -> &'static str {
    let lines: Vec<&'static str> = GREETINGS.lines().filter(|l| !l.trim().is_empty()).collect();
    let count = u64::try_from(lines.len()).unwrap_or(1).max(1);
    let index = usize::try_from(key % count).unwrap_or(0);
    lines.get(index).copied().unwrap_or("Good day.")
}

/// Fills `{{key}}` placeholders. Unknown placeholders are left as they are
/// so a typo shows up in the output instead of vanishing.
#[must_use]
pub fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_owned();
    for (key, value) in values {
        out = out.replace(&format!("{{{{{key}}}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_replaced() {
        assert_eq!(
            render("Hi {{name}}, {{name}}! {{missing}}", &[("name", "Henk")]),
            "Hi Henk, Henk! {{missing}}"
        );
    }

    #[test]
    fn persona_is_in_style() {
        assert!(henk_domain::text::is_in_style(PERSONA));
        assert!(henk_domain::text::is_in_style(PLANNER));
        assert!(henk_domain::text::is_in_style(SKILLS));
        assert!(henk_domain::text::is_in_style(EXECUTOR_BASH));
        assert!(henk_domain::text::is_in_style(PLAN_WORKSPACE));
        assert!(henk_domain::text::is_in_style(LOOP_REVIEWER));
        assert!(henk_domain::text::is_in_style(LOOP_EXECUTOR));
        for tool in ["report_finding", "finish_round", "reopens"] {
            assert!(LOOP_REVIEWER.contains(&format!("`{tool}`")), "{tool}");
        }
        for tool in ["list_findings", "give_verdict", "run_checks"] {
            assert!(LOOP_EXECUTOR.contains(&format!("`{tool}`")), "{tool}");
        }
        assert!(EXECUTOR_BASH.contains("`bash`") && EXECUTOR_BASH.contains("`mktemp`"));
        assert!(PLAN_WORKSPACE.contains("{{tools}}"));
        assert!(PLAN_WORKSPACE.contains("`list_commits`"));
        // The planner's copy is one commit without history (#183).
        assert!(PLAN_WORKSPACE.contains("without history"));
        assert!(!PLAN_WORKSPACE.contains("`git log` on a file"));
        assert!(henk_domain::text::is_in_style(GREETINGS));
        assert!(greeting(7).contains("review"));
        assert_eq!(greeting(1), greeting(1 + 4));
        assert!(PERSONA.contains("Not bad."));
    }

    #[test]
    fn skills_block_names_its_tool_and_placeholder() {
        assert!(SKILLS.contains("`load_skill`"));
        assert!(SKILLS.contains("{{catalogue}}"));
    }
}
