//! Tool names as the model sees them.
//!
//! Every tool a lane can call is `<server>__<tool>`, so two servers with a
//! tool of the same name never collide, and so the dispatcher can route a
//! call back to its server. Characters outside `[A-Za-z0-9_-]` are replaced
//! and the result is capped at 64 bytes, which every provider accepts.

use std::collections::HashMap;

/// Separator between the server alias and the tool name.
pub const SEPARATOR: &str = "__";

const MAX_LEN: usize = 64;

/// Where a model-facing tool name points.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolOrigin {
    /// The configured alias of the server.
    pub server: String,
    /// The tool name as the server declares it.
    pub tool: String,
}

/// A two-way map between model-facing names and server tools.
#[derive(Debug, Default, Clone)]
pub struct NameMap {
    forward: HashMap<ToolOrigin, String>,
    reverse: HashMap<String, ToolOrigin>,
}

impl NameMap {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool and returns the name the model will see. Registering
    /// the same origin twice returns the same name. When two distinct origins
    /// would collide after sanitising, the second gets a numeric suffix.
    pub fn register(&mut self, server: &str, tool: &str) -> String {
        let origin = ToolOrigin {
            server: server.to_owned(),
            tool: tool.to_owned(),
        };
        if let Some(existing) = self.forward.get(&origin) {
            return existing.clone();
        }
        let base = sanitize(&format!("{server}{SEPARATOR}{tool}"));
        let mut candidate = base.clone();
        let mut suffix = 2_u32;
        while self.reverse.contains_key(&candidate) {
            let tail = format!("_{suffix}");
            let mut head = base.clone();
            truncate_to(&mut head, MAX_LEN.saturating_sub(tail.len()));
            candidate = format!("{head}{tail}");
            suffix += 1;
        }
        self.forward.insert(origin.clone(), candidate.clone());
        self.reverse.insert(candidate.clone(), origin);
        candidate
    }

    /// The server and tool behind a model-facing name.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&ToolOrigin> {
        self.reverse.get(name)
    }

    /// The model-facing name of a server tool, if registered.
    #[must_use]
    pub fn name_of(&self, server: &str, tool: &str) -> Option<&str> {
        let origin = ToolOrigin {
            server: server.to_owned(),
            tool: tool.to_owned(),
        };
        self.forward.get(&origin).map(String::as_str)
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.reverse.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }
}

/// Replaces anything outside `[A-Za-z0-9_-]` with `_` and caps the length.
#[must_use]
pub fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("tool");
    }
    truncate_to(&mut out, MAX_LEN);
    out
}

fn truncate_to(s: &mut String, max: usize) {
    if s.len() > max {
        s.truncate(max);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn names_are_prefixed_and_resolve_back() {
        let mut map = NameMap::new();
        let name = map.register("github", "get_pull_request");
        assert_eq!(name, "github__get_pull_request");
        assert_eq!(
            map.resolve(&name),
            Some(&ToolOrigin {
                server: "github".into(),
                tool: "get_pull_request".into()
            })
        );
        assert_eq!(
            map.name_of("github", "get_pull_request"),
            Some(name.as_str())
        );
        assert_eq!(
            map.register("github", "get_pull_request"),
            name,
            "idempotent"
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn invalid_characters_are_replaced_and_length_capped() {
        assert_eq!(sanitize("a.b c/d"), "a_b_c_d");
        assert_eq!(sanitize(""), "tool");
        assert_eq!(sanitize(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn collisions_after_sanitising_get_a_suffix() {
        let mut map = NameMap::new();
        let first = map.register("s", "a.b");
        let second = map.register("s", "a_b");
        assert_eq!(first, "s__a_b");
        assert_eq!(second, "s__a_b_2");
        assert_eq!(map.resolve(&second).unwrap().tool, "a_b");
    }

    #[test]
    fn long_names_with_suffix_stay_within_limit() {
        let mut map = NameMap::new();
        let long = "y".repeat(80);
        let first = map.register("s", &long);
        let second = map.register("t", &"z".repeat(80));
        assert!(first.len() <= 64);
        assert!(second.len() <= 64);
        assert_ne!(first, second);
    }
}
