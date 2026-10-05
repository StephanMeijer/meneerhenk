//! Prompt templates, compiled into the binary.

/// Henk's persona (spec §7). Prepended to every system prompt.
pub const PERSONA: &str = include_str!("../prompts/persona.md");

/// Instructions for one review lane (§3.2).
pub const REVIEW_LANE: &str = include_str!("../prompts/review_lane.md");

/// Instructions for a fact-check of one finding (§3.2).
pub const FACT_CHECK: &str = include_str!("../prompts/fact_check.md");

/// Instructions for the planner (§4).
pub const PLANNER: &str = include_str!("../prompts/planner.md");

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
        assert!(henk_domain::text::is_in_style(REVIEW_LANE));
        assert!(henk_domain::text::is_in_style(FACT_CHECK));
        assert!(henk_domain::text::is_in_style(PLANNER));
        assert!(henk_domain::text::is_in_style(GREETINGS));
        assert!(greeting(7).contains("review"));
        assert_eq!(greeting(1), greeting(1 + 4));
        assert!(PERSONA.contains("Not bad."));
    }

    #[test]
    fn review_lane_names_every_lane_tool() {
        for tool in [
            "list_changed_files",
            "get_file_diff",
            "read_file",
            "list_existing_findings",
            "post_finding",
            "improve_finding",
            "withdraw_finding",
        ] {
            assert!(REVIEW_LANE.contains(&format!("`{tool}`")), "{tool}");
        }
    }

    #[test]
    fn fact_check_names_every_checker_tool() {
        for tool in [
            "list_changed_files",
            "get_file_diff",
            "read_file",
            "give_verdict",
        ] {
            assert!(FACT_CHECK.contains(&format!("`{tool}`")), "{tool}");
        }
    }
}
