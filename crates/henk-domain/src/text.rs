//! Style rules for Henk's own text (§7): no emoji, no em-dash.
//!
//! A platform reaction such as 👀 is an acknowledgement, not text, and is not
//! checked here. Typographic symbols that are emoji only when followed by a
//! variation selector (™ © ↔ ‼) are prose and pass; with the selector they
//! are caught by it.

/// A style rule a piece of text breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleViolation {
    /// An em-dash (U+2014).
    EmDash {
        /// Byte offset of the character.
        at: usize,
    },
    /// An emoji or pictograph.
    Emoji {
        /// Byte offset of the character.
        at: usize,
        /// The character.
        character: char,
    },
}

fn is_emoji(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1F000..=0x1FAFF   // mahjong, cards, emoticons, symbols, pictographs
            | 0x2600..=0x27BF // miscellaneous symbols, dingbats
            | 0x2B00..=0x2BFF // arrows and shapes
            | 0xFE0F          // variation selector 16
            | 0x20E3          // combining keycap
            | 0x231A..=0x231B // watch, hourglass
            | 0x23E9..=0x23F3 // media and timer symbols
            | 0x23F8..=0x23FA // pause, stop, record
            | 0x25FD..=0x25FE // medium small squares
            | 0x3030 | 0x303D // wavy dash, part alternation mark
            | 0x3297 | 0x3299 // circled ideographs
    )
}

/// Finds every style rule the text breaks, in order of appearance.
#[must_use]
pub fn style_violations(text: &str) -> Vec<StyleViolation> {
    text.char_indices()
        .filter_map(|(at, c)| match c {
            '\u{2014}' => Some(StyleViolation::EmDash { at }),
            c if is_emoji(c) => Some(StyleViolation::Emoji { at, character: c }),
            _ => None,
        })
        .collect()
}

/// Whether the text follows the style rules.
#[must_use]
pub fn is_in_style(text: &str) -> bool {
    style_violations(text).is_empty()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn plain_prose_passes() {
        assert!(is_in_style(
            "Not bad. Tests are not optional; add one for the empty case."
        ));
        assert!(is_in_style("It giet oan! Ümlauts and hyphens - fine."));
    }

    #[test]
    fn em_dash_and_emoji_are_flagged() {
        assert_eq!(
            style_violations("Fine \u{2014} not."),
            vec![StyleViolation::EmDash { at: 5 }]
        );
        assert_eq!(
            style_violations("Looks good 👀"),
            vec![StyleViolation::Emoji {
                at: 11,
                character: '👀'
            }]
        );
    }

    /// #52: one character per range, so dropping a range fails here.
    #[test]
    fn every_emoji_range_is_flagged() {
        for text in [
            "\u{1F004}",
            "👀",
            "☀",
            "✅",
            "⭐",
            "⌚",
            "⌛",
            "⏩",
            "⏰",
            "⏳",
            "⏸",
            "◽",
            "◾",
            "〰",
            "〽",
            "㊗",
            "㊙",
            "1\u{20E3}",
            "™\u{FE0F}",
        ] {
            let found = style_violations(text);
            assert!(
                found
                    .iter()
                    .any(|v| matches!(v, StyleViolation::Emoji { .. })),
                "{text:?} ({:X?}) is not flagged",
                text.chars().map(u32::from).collect::<Vec<_>>()
            );
        }
    }

    /// The other side of the boundary: ordinary typographic symbols are
    /// prose, not emoji.
    #[test]
    fn typographic_symbols_are_not_emoji() {
        for text in [
            "™", "©", "®", "↔", "→", "▪", "‼", "ℹ", "⌘", "¶", "§", "°", "€", "1.",
        ] {
            assert!(is_in_style(text), "{text:?} is flagged");
        }
    }
}
