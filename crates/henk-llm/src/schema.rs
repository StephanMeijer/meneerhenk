//! Cleaning JSON Schemas before they go to a provider.
//!
//! MCP servers emit schemas with `$schema` keys and sometimes without a top
//! level `type`. Some proxies reject the first and some providers the second.

use serde_json::{Map, Value};

/// Returns a schema every provider accepts as tool parameters.
///
/// - A missing, null or non-object schema becomes an empty object schema.
/// - `$schema` is removed at the top level.
/// - A top level without `type` gets `"type": "object"`.
/// - A top level object schema without `properties` gets an empty one.
#[must_use]
pub fn clean(schema: &Value) -> Value {
    let mut object = match schema {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    object.remove("$schema");
    object
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    if object.get("type").and_then(Value::as_str) == Some("object") {
        object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
    }
    Value::Object(object)
}

/// OpenAI rejects a function description over 1024 characters, and several
/// MCP servers ship longer ones. Cuts at a character boundary with a marker.
#[must_use]
pub fn cap_description(description: &str) -> String {
    const LIMIT: usize = 1024;
    const MARKER: &str = " [...]";
    if description.chars().count() <= LIMIT {
        return description.to_owned();
    }
    let keep: String = description.chars().take(LIMIT - MARKER.len()).collect();
    format!("{}{MARKER}", keep.trim_end())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn null_becomes_empty_object_schema() {
        assert_eq!(
            clean(&Value::Null),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn dollar_schema_is_removed_and_type_added() {
        let input = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "properties": {"owner": {"type": "string"}},
            "required": ["owner"]
        });
        assert_eq!(
            clean(&input),
            json!({
                "type": "object",
                "properties": {"owner": {"type": "string"}},
                "required": ["owner"]
            })
        );
    }

    #[test]
    fn existing_type_is_kept() {
        let input = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        assert_eq!(clean(&input), input);
    }
}
