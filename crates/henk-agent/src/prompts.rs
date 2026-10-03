//! Prompt templates, compiled into the binary.

/// Henk's persona (spec §7). Prepended to every system prompt.
pub const PERSONA: &str = include_str!("../prompts/persona.md");

/// Instructions for one review lane (§3.2).
pub const REVIEW_LANE: &str = include_str!("../prompts/review_lane.md");

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
        assert!(PERSONA.contains("Not bad."));
    }
}
