//! Keeping a conversation under a size budget.
//!
//! Every turn resends the whole conversation, so a lane that reads one big
//! file per turn pays for all of them on every later turn. Once the
//! conversation is over budget, the oldest large tool results are replaced
//! by a one-line stub that names the tool and the turn; the model can call
//! the tool again if it needs the content. The system prompt, the opening
//! messages, the model's own text and the most recent turns are never
//! touched.

use std::fmt::Write as _;

use henk_llm::{Block, ChatMessage, Role, ToolArguments};

/// Results shorter than this are never stubbed; the stub would not be
/// much shorter.
const MIN_STUB_CHARS: usize = 200;

/// Characters the conversation takes as the model sees it: text, tool call
/// arguments and tool results.
#[must_use]
pub fn size(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .flat_map(|m| &m.blocks)
        .map(|block| match block {
            Block::Text(text) => text.chars().count(),
            Block::ToolCall(call) => match &call.arguments {
                ToolArguments::Parsed(value) => value.to_string().chars().count(),
                ToolArguments::Malformed(raw) => raw.chars().count(),
            },
            Block::ToolResult(result) => result.content.chars().count(),
            Block::Opaque(value) => value.to_string().chars().count(),
        })
        .sum()
}

/// Brings `messages` under `budget` characters by stubbing old tool
/// results, oldest first, leaving the last `keep_recent_turns` turns intact.
/// A turn starts at an assistant message. Returns how many results were
/// stubbed.
pub fn compact(messages: &mut [ChatMessage], budget: usize, keep_recent_turns: u32) -> usize {
    let mut total = size(messages);
    if total <= budget {
        return 0;
    }
    // The index of the first message of the turns to keep intact.
    let assistant_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::Assistant)
        .map(|(i, _)| i)
        .collect();
    let keep = usize::try_from(keep_recent_turns).unwrap_or(usize::MAX);
    let protected_from = if keep == 0 {
        messages.len()
    } else {
        assistant_indices
            .len()
            .checked_sub(keep)
            .and_then(|at| assistant_indices.get(at).copied())
            .unwrap_or(0)
    };

    let mut stubbed = 0;
    let mut turn = 0;
    let mut last_calls: Vec<(String, String)> = Vec::new();
    for (index, message) in messages.iter_mut().enumerate() {
        if message.role == Role::Assistant {
            turn += 1;
            last_calls = message
                .tool_calls()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect();
            continue;
        }
        if index >= protected_from {
            break;
        }
        for block in &mut message.blocks {
            if total <= budget {
                return stubbed;
            }
            let Block::ToolResult(result) = block else {
                continue;
            };
            let chars = result.content.chars().count();
            if chars < MIN_STUB_CHARS || result.content.starts_with("[result of ") {
                continue;
            }
            let tool = last_calls
                .iter()
                .find(|(id, _)| *id == result.call_id)
                .map_or("a tool", |(_, name)| name.as_str());
            let mut stub = String::new();
            let _ = write!(
                stub,
                "[result of {tool} from turn {turn} elided ({chars} chars); call it again if you need it]"
            );
            total = total.saturating_sub(chars) + stub.chars().count();
            result.content = stub;
            stubbed += 1;
        }
    }
    stubbed
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use henk_llm::{ToolCall, ToolResult};
    use serde_json::json;

    use super::*;

    fn turn(n: usize, chars: usize) -> [ChatMessage; 2] {
        let id = format!("c{n}");
        [
            ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall(ToolCall {
                    id: id.clone(),
                    name: format!("tool{n}"),
                    arguments: ToolArguments::Parsed(json!({})),
                })],
            },
            ChatMessage::tool_results([ToolResult {
                call_id: id,
                content: "x".repeat(chars),
                is_error: false,
            }]),
        ]
    }

    fn conversation(turns: &[usize]) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::user("go")];
        for (n, chars) in turns.iter().enumerate() {
            messages.extend(turn(n + 1, *chars));
        }
        messages
    }

    #[test]
    fn under_budget_nothing_changes() {
        let mut messages = conversation(&[1000, 1000]);
        let before = messages.clone();
        assert_eq!(compact(&mut messages, 10_000, 2), 0);
        assert_eq!(messages, before);
    }

    #[test]
    fn stubs_the_oldest_large_results_and_keeps_recent_turns() {
        let mut messages = conversation(&[5000, 100, 5000, 5000, 5000]);
        let stubbed = compact(&mut messages, 12_000, 2);
        assert_eq!(
            stubbed, 2,
            "turns 1 and 3; turn 2 is small, 4 and 5 are recent"
        );
        let result = |i: usize| match &messages[i].blocks[0] {
            Block::ToolResult(r) => r.content.clone(),
            other => panic!("{other:?}"),
        };
        assert!(result(2).starts_with("[result of tool1 from turn 1 elided (5000 chars)"));
        assert_eq!(result(4).len(), 100);
        assert!(result(6).starts_with("[result of tool3 from turn 3 elided"));
        assert_eq!(result(8).len(), 5000);
        assert_eq!(result(10).len(), 5000);
        assert!(size(&messages) <= 12_000);
        assert_eq!(messages[0].text(), "go");
        // Call ids survive so providers still see each result paired.
        assert!(matches!(&messages[2].blocks[0], Block::ToolResult(r) if r.call_id == "c1"));
    }

    #[test]
    fn stops_as_soon_as_the_budget_is_met_and_never_stubs_twice() {
        let mut messages = conversation(&[5000, 5000, 5000, 5000]);
        assert_eq!(compact(&mut messages, 16_000, 1), 1);
        assert_eq!(compact(&mut messages, 16_000, 1), 0, "already under budget");
        assert_eq!(
            compact(&mut messages, 6_000, 1),
            2,
            "stubs are skipped, the next two go"
        );
    }

    #[test]
    fn zero_recent_turns_protects_nothing() {
        let mut messages = conversation(&[5000, 5000]);
        assert_eq!(compact(&mut messages, 100, 0), 2);
        assert!(size(&messages) < 300);
    }
}
