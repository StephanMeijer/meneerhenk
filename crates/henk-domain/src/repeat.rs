//! The repeat guard: a model that calls the same tool with the same
//! arguments over and over is stuck, and the result will not change. After
//! `limit` identical calls in a row the next one is refused; one more, made
//! in a later turn after the model has seen the refusal, ends the session.
//! Each firing is recorded on the run (§8.6).

use serde_json::Value;

/// The default number of identical calls in a row that are let through.
pub const DEFAULT_LIMIT: u32 = 3;

/// A tool call's arguments as the model sent them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallArguments<'a> {
    /// Valid JSON. Compared with object keys in sorted order, so key order
    /// does not make two calls different.
    Json(&'a Value),
    /// Text that is not JSON, compared as it is.
    Raw(&'a str),
}

/// What to do with a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatVerdict {
    /// Run it.
    Allow,
    /// Do not run it; tell the model to change course.
    Refuse {
        /// Identical calls in a row, this one included.
        repeats: u32,
    },
    /// The model repeated the call after a refusal; end the session.
    Stuck {
        /// Identical calls in a row, this one included.
        repeats: u32,
    },
}

/// Watches the tool calls of one session, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatGuard {
    limit: u32,
    last: Option<(String, String)>,
    streak: u32,
    refused: Refused,
}

/// Whether the current streak was refused, and whether the model has seen
/// it. Results go back to the model only after a whole turn, so a call in
/// the turn that holds the refusal was made without knowing of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    /// Not refused.
    No,
    /// Refused in the turn still running.
    ThisTurn,
    /// Refused in an earlier turn; the model has seen it.
    Seen,
}

impl RepeatGuard {
    /// A guard that lets `limit` identical calls in a row through. A limit
    /// of 0 turns the guard off.
    #[must_use]
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            last: None,
            streak: 0,
            refused: Refused::No,
        }
    }

    /// Records a call and says what to do with it. A refused call still
    /// counts. An identical call in a later turn than the refusal, once the
    /// model has seen it, is the stuck one; in the refusal's own turn it is
    /// refused too.
    pub fn observe(&mut self, tool: &str, arguments: CallArguments<'_>) -> RepeatVerdict {
        if self.limit == 0 {
            return RepeatVerdict::Allow;
        }
        let key = canonical(arguments);
        let same = self
            .last
            .as_ref()
            .is_some_and(|(last_tool, last_key)| last_tool == tool && *last_key == key);
        if same {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.last = Some((tool.to_owned(), key));
            self.streak = 1;
            self.refused = Refused::No;
        }
        let repeats = self.streak;
        if repeats <= self.limit {
            RepeatVerdict::Allow
        } else if self.refused == Refused::Seen {
            RepeatVerdict::Stuck { repeats }
        } else {
            self.refused = Refused::ThisTurn;
            RepeatVerdict::Refuse { repeats }
        }
    }

    /// Marks the end of a turn: its results, refusals included, go back to
    /// the model.
    pub fn end_turn(&mut self) {
        if self.refused == Refused::ThisTurn {
            self.refused = Refused::Seen;
        }
    }
}

/// What the model is told in place of a refused call's result.
#[must_use]
pub fn refusal_message(tool: &str, repeats: u32) -> String {
    format!(
        "Refused: you called {tool} with the same arguments {repeats} times in a row, \
         and the result will not change. Try something else, or finish with what you have. \
         Calling it again with these arguments ends the session."
    )
}

/// What the model is told in place of the call that ended the session.
#[must_use]
pub fn stuck_message(tool: &str, repeats: u32) -> String {
    format!(
        "Not run: you called {tool} with the same arguments {repeats} times in a row \
         after being told to stop, so the session is ending."
    )
}

/// The arguments as text that is equal for equal calls.
fn canonical(arguments: CallArguments<'_>) -> String {
    match arguments {
        CallArguments::Json(value) => {
            let mut out = String::new();
            write_canonical(value, &mut out);
            out
        }
        CallArguments::Raw(raw) => format!("raw:{raw}"),
    }
}

/// Writes `value` as JSON with object keys sorted at every level, without
/// relying on the order `serde_json` keeps its maps in.
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;
    use crate::text::style_violations;

    fn verdicts(guard: &mut RepeatGuard, calls: &[(&str, Value)]) -> Vec<RepeatVerdict> {
        calls
            .iter()
            .map(|(tool, args)| guard.observe(tool, CallArguments::Json(args)))
            .collect()
    }

    /// One call per turn.
    fn turns(guard: &mut RepeatGuard, calls: &[(&str, Value)]) -> Vec<RepeatVerdict> {
        calls
            .iter()
            .map(|(tool, args)| {
                let verdict = guard.observe(tool, CallArguments::Json(args));
                guard.end_turn();
                verdict
            })
            .collect()
    }

    #[test]
    fn refused_after_the_limit_then_stuck() {
        let mut guard = RepeatGuard::new(3);
        let call = ("read_file", json!({"path": "a.rs"}));
        let got = turns(&mut guard, &vec![call; 5]);
        assert_eq!(
            got,
            vec![
                RepeatVerdict::Allow,
                RepeatVerdict::Allow,
                RepeatVerdict::Allow,
                RepeatVerdict::Refuse { repeats: 4 },
                RepeatVerdict::Stuck { repeats: 5 },
            ]
        );
    }

    #[test]
    fn repeats_in_the_refusal_turn_are_refused_not_stuck() {
        let mut guard = RepeatGuard::new(3);
        let call = ("read_file", json!({"path": "a.rs"}));
        let mut got = turns(&mut guard, &vec![call.clone(); 3]);
        // Turn 4 holds two identical calls: the model has not seen the
        // first refusal when it makes the second.
        got.extend(verdicts(&mut guard, &[call.clone(), call.clone()]));
        guard.end_turn();
        got.extend(verdicts(&mut guard, &[call]));
        assert_eq!(
            got,
            vec![
                RepeatVerdict::Allow,
                RepeatVerdict::Allow,
                RepeatVerdict::Allow,
                RepeatVerdict::Refuse { repeats: 4 },
                RepeatVerdict::Refuse { repeats: 5 },
                RepeatVerdict::Stuck { repeats: 6 },
            ]
        );
    }

    #[test]
    fn a_new_streak_needs_a_new_refusal() {
        let mut guard = RepeatGuard::new(1);
        let same = ("read_file", json!({"path": "a.rs"}));
        let other = ("list_files", json!({}));
        let got = turns(
            &mut guard,
            &[same.clone(), same.clone(), other, same.clone(), same],
        );
        assert_eq!(
            got,
            vec![
                RepeatVerdict::Allow,
                RepeatVerdict::Refuse { repeats: 2 },
                RepeatVerdict::Allow,
                RepeatVerdict::Allow,
                RepeatVerdict::Refuse { repeats: 2 },
            ]
        );
    }

    #[test]
    fn different_arguments_do_not_trigger_it() {
        let mut guard = RepeatGuard::new(2);
        let calls: Vec<_> = (0..6)
            .map(|i| ("read_file", json!({"path": format!("{i}.rs")})))
            .collect();
        assert!(
            verdicts(&mut guard, &calls)
                .iter()
                .all(|v| *v == RepeatVerdict::Allow)
        );
    }

    #[test]
    fn another_tool_with_the_same_arguments_is_a_different_call() {
        let mut guard = RepeatGuard::new(1);
        let args = json!({"path": "a.rs"});
        let calls = [("read_file", args.clone()), ("get_file_diff", args)];
        assert!(
            verdicts(&mut guard, &calls)
                .iter()
                .all(|v| *v == RepeatVerdict::Allow)
        );
    }

    #[test]
    fn a_call_in_between_resets_the_streak() {
        let mut guard = RepeatGuard::new(2);
        let same = ("read_file", json!({"path": "a.rs"}));
        let other = ("list_files", json!({}));
        let calls = [
            same.clone(),
            same.clone(),
            other,
            same.clone(),
            same.clone(),
        ];
        assert!(
            verdicts(&mut guard, &calls)
                .iter()
                .all(|v| *v == RepeatVerdict::Allow)
        );
        assert_eq!(
            verdicts(&mut guard, &[same]),
            vec![RepeatVerdict::Refuse { repeats: 3 }]
        );
    }

    #[test]
    fn key_order_does_not_matter() {
        let mut guard = RepeatGuard::new(1);
        let a: Value =
            serde_json::from_str(r#"{"a":1,"b":{"x":[1,{"q":2,"p":1}],"y":2}}"#).unwrap();
        let b: Value =
            serde_json::from_str(r#"{"b":{"y":2,"x":[1,{"p":1,"q":2}]},"a":1}"#).unwrap();
        assert_eq!(
            guard.observe("t", CallArguments::Json(&a)),
            RepeatVerdict::Allow
        );
        assert_eq!(
            guard.observe("t", CallArguments::Json(&b)),
            RepeatVerdict::Refuse { repeats: 2 }
        );
    }

    #[test]
    fn array_order_and_value_types_still_matter() {
        assert_ne!(
            canonical(CallArguments::Json(&json!([1, 2]))),
            canonical(CallArguments::Json(&json!([2, 1])))
        );
        assert_ne!(
            canonical(CallArguments::Json(&json!({"n": 1}))),
            canonical(CallArguments::Json(&json!({"n": "1"})))
        );
    }

    #[test]
    fn malformed_arguments_are_compared_as_raw_text() {
        let mut guard = RepeatGuard::new(1);
        assert_eq!(
            guard.observe("t", CallArguments::Raw("{path: a")),
            RepeatVerdict::Allow
        );
        assert_eq!(
            guard.observe("t", CallArguments::Raw("{path: b")),
            RepeatVerdict::Allow
        );
        assert_eq!(
            guard.observe("t", CallArguments::Raw("{path: b")),
            RepeatVerdict::Refuse { repeats: 2 }
        );
        // Raw text that happens to read as a JSON string is still raw.
        assert_eq!(
            guard.observe("t", CallArguments::Json(&json!("{path: b"))),
            RepeatVerdict::Allow
        );
    }

    #[test]
    fn a_limit_of_zero_never_fires() {
        let mut guard = RepeatGuard::new(0);
        let call = ("read_file", json!({"path": "a.rs"}));
        assert!(
            verdicts(&mut guard, &vec![call; 10])
                .iter()
                .all(|v| *v == RepeatVerdict::Allow)
        );
    }

    #[test]
    fn the_messages_are_in_style() {
        assert!(style_violations(&refusal_message("read_file", 4)).is_empty());
        assert!(style_violations(&stuck_message("read_file", 5)).is_empty());
        assert!(refusal_message("read_file", 4).contains("read_file"));
        assert!(stuck_message("read_file", 5).contains("ending"));
    }
}
