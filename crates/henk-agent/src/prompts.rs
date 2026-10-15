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

/// Instructions for the planner (§4), who drafts and revises after the
/// reviewer's points (#328).
pub const PLANNER: &str = include_str!("../prompts/planner.md");

/// Instructions for the reviewer of a drafted plan (#328). `{{ref}}`,
/// `{{repo}}` and `{{workspace}}`, what the reviewer's copy of the
/// repository is, are filled in.
pub const PLAN_REVIEWER: &str = include_str!("../prompts/plan_reviewer.md");

/// The plan reviewer's `{{workspace}}` when it has a copy of the
/// repository.
pub const PLAN_REVIEWER_WORKSPACE: &str = "You read the code in the planner's copy of the repository at its default branch, {{branch}} at {{commit}}, with `list_files`, `search`, `read_file` and `bash`; nothing you change there goes anywhere.";

/// The plan reviewer's `{{workspace}}` without one.
pub const PLAN_REVIEWER_NO_WORKSPACE: &str = "Read the code with the platform's tools.";

/// Instructions for the reviewer of a review loop (#284). `{{ref}}` and
/// `{{repo}}` say which pull request.
pub const LOOP_REVIEWER: &str = include_str!("../prompts/loop_reviewer.md");

/// Instructions for the executor of a review loop (#284). `{{ref}}`,
/// `{{repo}}`, `{{max_files}}` and `{{recent_commits}}`, the branch's
/// latest commit subjects (#400), are filled in.
pub const LOOP_EXECUTOR: &str = include_str!("../prompts/loop_executor.md");

/// Instructions for the executor taking an issue to a pull request (#329).
/// `{{ref}}`, `{{repo}}`, `{{branch}}`, `{{max_files}}` and
/// `{{recent_commits}}`, the default branch's latest commit subjects
/// (#400), are filled in.
pub const EXECUTE: &str = include_str!("../prompts/execute.md");

/// Instructions for the executor resolving a rebase's conflicts (#579).
/// `{{ref}}`, `{{repo}}`, `{{base}}`, `{{commit}}` (the subject of the
/// commit being replayed), `{{files}}` (the conflicted files, one per line)
/// and `{{edit_tool}}` are filled in.
pub const REBASE: &str = include_str!("../prompts/rebase.md");

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
        assert!(henk_domain::text::is_in_style(PLAN_REVIEWER));
        assert!(henk_domain::text::is_in_style(EXECUTE));
        assert!(henk_domain::text::is_in_style(REBASE));
        for placeholder in ["{{base}}", "{{files}}", "{{edit_tool}}", "`write_file`"] {
            assert!(REBASE.contains(placeholder), "{placeholder}");
        }
        for tool in ["edit_file", "propose_pull_request"] {
            assert!(EXECUTE.contains(&format!("`{tool}`")), "{tool}");
        }
        assert!(henk_domain::text::is_in_style(PLAN_REVIEWER_WORKSPACE));
        for tool in ["raise_point", "finish_round", "reopens"] {
            assert!(PLAN_REVIEWER.contains(&format!("`{tool}`")), "{tool}");
        }
        for tool in ["answer_point", "withdraw_change", "write_plan"] {
            assert!(PLANNER.contains(&format!("`{tool}`")), "{tool}");
        }
        for tool in ["report_finding", "finish_round", "reopens"] {
            assert!(LOOP_REVIEWER.contains(&format!("`{tool}`")), "{tool}");
        }
        for tool in ["list_findings", "give_verdict", "propose_commits"] {
            assert!(LOOP_EXECUTOR.contains(&format!("`{tool}`")), "{tool}");
        }
        // Each executor finds the repository's commit convention, groups its
        // commits and leaves the trailers to Henk (#400).
        for prompt in [EXECUTE, LOOP_EXECUTOR] {
            for needle in [
                "{{recent_commits}}",
                "`commitlint",
                "`CONTRIBUTING.md`",
                "`AGENTS.md`",
                "Conventional Commits",
                "No trailers",
            ] {
                assert!(prompt.contains(needle), "{needle} in {prompt}");
            }
        }
        assert!(EXECUTE.contains("`commits`"));
        // Henk's own checks are gone; the executor runs the repository's
        // with bash, and says which (#396).
        for prompt in [EXECUTE, LOOP_EXECUTOR, EXECUTOR_BASH] {
            assert!(!prompt.contains("run_checks"), "{prompt}");
        }
        assert!(EXECUTOR_BASH.contains("`bash`") && EXECUTOR_BASH.contains("`mktemp`"));
        for source in [
            "`.github/workflows`",
            "`.gitlab-ci.yml`",
            "`Makefile`",
            "`AGENTS.md`",
        ] {
            assert!(EXECUTOR_BASH.contains(source), "{source}");
        }
        assert!(EXECUTOR_BASH.contains("Name each check you ran"));
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
