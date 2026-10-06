//! Keeping a conversation under a size budget.
//!
//! Every turn resends the whole conversation, so a lane that reads one big
//! file per turn pays for all of them on every later turn. Once the
//! conversation is over budget, the oldest large tool results are replaced
//! by a one-line stub that names the tool and the turn; the model can call
//! the tool again if it needs the content. Results of tools that mark
//! themselves as the work itself ([`crate::Tool::keep_in_context`], the
//! review's diffs) go last: every other result is stubbed first. The system
//! prompt, the opening messages, the model's own text and the most recent
//! turns are never touched.
//!
//! Stubbing edits earlier turns, and current Claude models bind each
//! thinking block to the exact history before it: a replayed block after an
//! edit is refused. So whenever anything is stubbed, every provider-specific
//! block (thinking) is dropped from the whole conversation. Dropping all of
//! them is allowed; the model answers on without that earlier reasoning.
//!
//! An edit is expensive in two ways: it restarts every prefix cache (the
//! provider's own, or Anthropic's explicit one) and it costs the model its
//! earlier reasoning. So once over budget, compaction stubs down to a
//! low-water mark well under the budget, not to the budget itself: the
//! history then stays append-only for several turns before the next edit,
//! instead of being rewritten on every turn (#54).

use std::fmt::Write as _;

use henk_llm::{Block, ChatMessage, Role, ToolArguments};

/// Results shorter than this are never stubbed; the stub would not be
/// much shorter.
const MIN_STUB_CHARS: usize = 200;

/// Once over budget, stub down to this share of it, in percent.
const LOW_WATER_PERCENT: usize = 75;

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

/// When `messages` is over `budget` characters, brings it down to the
/// low-water mark ([`LOW_WATER_PERCENT`] of the budget) by stubbing old tool
/// results, oldest first, leaving the last `keep_recent_turns` turns intact.
/// A turn starts at an assistant message. Returns how many results were
/// stubbed. When that is more than zero, opaque blocks are dropped too.
pub fn compact(
    messages: &mut [ChatMessage],
    budget: usize,
    keep_recent_turns: u32,
    keep: impl Fn(&str) -> bool,
) -> usize {
    let stubbed = stub_old_results(messages, budget, keep_recent_turns, &keep);
    if stubbed > 0 {
        for message in messages.iter_mut() {
            message
                .blocks
                .retain(|block| !matches!(block, Block::Opaque(_)));
        }
    }
    stubbed
}

fn stub_old_results(
    messages: &mut [ChatMessage],
    budget: usize,
    keep_recent_turns: u32,
    keep: &dyn Fn(&str) -> bool,
) -> usize {
    let mut total = size(messages);
    if total <= budget {
        return 0;
    }
    let target = budget / 100 * LOW_WATER_PERCENT + budget % 100 * LOW_WATER_PERCENT / 100;
    // The index of the first message of the turns to keep intact.
    let assistant_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::Assistant)
        .map(|(i, _)| i)
        .collect();
    let recent = usize::try_from(keep_recent_turns).unwrap_or(usize::MAX);
    let protected_from = if recent == 0 {
        messages.len()
    } else {
        assistant_indices
            .len()
            .checked_sub(recent)
            .and_then(|at| assistant_indices.get(at).copied())
            .unwrap_or(0)
    };
    // Evidence first (file reads, MCP answers), then, only if that is not
    // enough, the results a tool marks as the work itself (the diffs).
    let mut stubbed = 0;
    for kept in [false, true] {
        stubbed += stub_pass(messages, protected_from, target, &mut total, |tool| {
            keep(tool) == kept
        });
    }
    stubbed
}

/// One oldest-first pass over the unprotected messages, stubbing the large
/// results of the tools `select` accepts until `total` is within `budget`.
fn stub_pass(
    messages: &mut [ChatMessage],
    protected_from: usize,
    budget: usize,
    total: &mut usize,
    select: impl Fn(&str) -> bool,
) -> usize {
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
            if *total <= budget {
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
            if !select(tool) {
                continue;
            }
            let mut stub = String::new();
            let _ = write!(
                stub,
                "[result of {tool} from turn {turn} elided ({chars} chars); call it again if you need it]"
            );
            *total = total.saturating_sub(chars) + stub.chars().count();
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
        assert_eq!(compact(&mut messages, 10_000, 2, |_| false), 0);
        assert_eq!(messages, before);
    }

    #[test]
    fn stubs_the_oldest_large_results_and_keeps_recent_turns() {
        let mut messages = conversation(&[5000, 100, 5000, 5000, 5000]);
        let stubbed = compact(&mut messages, 12_000, 2, |_| false);
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
    fn stops_at_the_low_water_mark_and_never_stubs_twice() {
        let mut messages = conversation(&[5000, 5000, 5000, 5000]);
        assert_eq!(
            compact(&mut messages, 16_000, 1, |_| false),
            2,
            "down to 12,000, not just under 16,000"
        );
        assert!(size(&messages) <= 12_000);
        assert_eq!(
            compact(&mut messages, 16_000, 1, |_| false),
            0,
            "already under budget"
        );
        assert_eq!(
            compact(&mut messages, 6_000, 1, |_| false),
            1,
            "stubs are skipped, only the third is left"
        );
    }

    #[test]
    fn stubbing_drops_every_thinking_block_and_nothing_else() {
        let thinking =
            || Block::Opaque(json!({"type": "thinking", "thinking": "", "signature": "s"}));
        let mut messages = conversation(&[5000, 5000, 5000]);
        messages[1].blocks.insert(0, thinking());
        messages[5].blocks.insert(0, thinking());
        assert_eq!(compact(&mut messages, 11_000, 1, |_| false), 2);
        assert!(
            messages
                .iter()
                .flat_map(|m| &m.blocks)
                .all(|b| !matches!(b, Block::Opaque(_))),
            "no thinking block survives an edit"
        );
        assert!(matches!(&messages[5].blocks[0], Block::ToolCall(_)));
    }

    #[test]
    fn thinking_blocks_stay_when_nothing_is_stubbed() {
        let mut messages = conversation(&[1000]);
        messages[1]
            .blocks
            .insert(0, Block::Opaque(json!({"type": "thinking"})));
        assert_eq!(compact(&mut messages, 100_000, 1, |_| false), 0);
        assert!(matches!(&messages[1].blocks[0], Block::Opaque(_)));
    }

    /// A turn whose one tool call is `tool`, with a result of `chars`.
    fn named_turn(n: usize, tool: &str, chars: usize) -> [ChatMessage; 2] {
        let [mut call, result] = turn(n, chars);
        if let Some(Block::ToolCall(c)) = call.blocks.first_mut() {
            c.name = tool.to_owned();
        }
        [call, result]
    }

    fn content(messages: &[ChatMessage], index: usize) -> String {
        match &messages[index].blocks[0] {
            Block::ToolResult(r) => r.content.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_kept_tools_result_outlives_a_newer_file_read() {
        // Turn 1 is a diff, turn 2 a file read, turns 3 and 4 are recent.
        let mut messages = vec![ChatMessage::user("go")];
        messages.extend(named_turn(1, "get_file_diff", 30_000));
        messages.extend(named_turn(2, "read_file", 30_000));
        messages.extend(named_turn(3, "read_file", 100));
        messages.extend(named_turn(4, "read_file", 100));
        let stubbed = compact(&mut messages, 45_000, 2, |tool| tool == "get_file_diff");
        assert_eq!(stubbed, 1);
        assert_eq!(content(&messages, 2).len(), 30_000, "the older diff stays");
        assert!(content(&messages, 4).starts_with("[result of read_file from turn 2 elided"));
    }

    #[test]
    fn kept_results_go_too_when_nothing_else_is_left() {
        let mut messages = vec![ChatMessage::user("go")];
        messages.extend(named_turn(1, "get_file_diff", 30_000));
        messages.extend(named_turn(2, "get_file_diff", 30_000));
        messages.extend(named_turn(3, "read_file", 30_000));
        messages.extend(named_turn(4, "read_file", 100));
        let stubbed = compact(&mut messages, 41_000, 1, |tool| tool == "get_file_diff");
        assert_eq!(stubbed, 2, "the read first, then the oldest diff");
        assert!(content(&messages, 6).starts_with("[result of read_file"));
        assert!(content(&messages, 2).starts_with("[result of get_file_diff from turn 1"));
        assert_eq!(content(&messages, 4).len(), 30_000, "the newer diff stays");
        assert!(size(&messages) <= 30_750, "the low-water mark of 41,000");
    }

    #[test]
    fn zero_recent_turns_protects_nothing() {
        let mut messages = conversation(&[5000, 5000]);
        assert_eq!(compact(&mut messages, 100, 0, |_| false), 2);
        assert!(size(&messages) < 300);
    }

    /// The hunt's scenario (#54): one 6,000-character file read per turn,
    /// a 160,000-character budget, two recent turns kept. Before the
    /// low-water mark, every turn past the budget rewrote earlier history.
    #[test]
    fn a_growing_conversation_is_edited_only_every_few_turns() {
        let mut messages = vec![ChatMessage::user("go")];
        let mut edited = Vec::new();
        for n in 1..=60 {
            messages.extend(turn(n, 6_000));
            if compact(&mut messages, 160_000, 2, |_| false) > 0 {
                edited.push(n);
            }
        }
        assert!(edited.len() >= 3, "it does compact: {edited:?}");
        for pair in edited.windows(2) {
            assert!(
                pair[1] - pair[0] >= 5,
                "edits at turns {edited:?}: history must stay append-only between them"
            );
        }
    }
}
