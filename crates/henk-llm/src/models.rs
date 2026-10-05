//! Listing the models an endpoint serves, for setup and `henk doctor`.

use secrecy::ExposeSecret as _;
use serde_json::Value;

use crate::client::{ModelConfig, Provider};
use crate::error::{LlmError, truncate_body};
use crate::http::{build_client, get_json, with_retry};

/// Lists the model ids the endpoint of `config` serves. The configured model
/// name itself is ignored; only the base URL, provider and key are used.
///
/// # Errors
///
/// Returns [`LlmError::InvalidConfig`] when no HTTP client can be built, the
/// usual HTTP errors from the endpoint, and [`LlmError::Decode`] when the
/// answer has no `data` array of objects with an `id`.
pub async fn list_models(config: &ModelConfig) -> Result<Vec<String>, LlmError> {
    let http = build_client(config.timeout)?;
    let url = match config.provider {
        Provider::OpenAi => format!("{}/models", config.base_url()),
        Provider::Anthropic => format!("{}/v1/models", config.base_url()),
    };
    with_retry(config.retry, |_| async {
        let builder = match config.provider {
            Provider::OpenAi => http.get(&url).bearer_auth(config.api_key.expose_secret()),
            Provider::Anthropic => http
                .get(&url)
                .header("x-api-key", config.api_key.expose_secret())
                .header("anthropic-version", crate::anthropic::API_VERSION),
        };
        let response = get_json(builder).await?;
        decode(&response)
    })
    .await
}

/// Both wire formats answer `{"data": [{"id": ...}, ...]}`.
fn decode(response: &Value) -> Result<Vec<String>, LlmError> {
    let data = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            LlmError::Decode(format!(
                "no data array in model list: {}",
                truncate_body(&response.to_string())
            ))
        })?;
    let mut ids: Vec<String> = data
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_sorted_unique_ids() {
        let ids =
            decode(&json!({"data": [{"id": "b"}, {"id": "a"}, {"id": "b"}, {"x": 1}]})).unwrap();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn missing_data_is_a_decode_error_with_body() {
        let error = decode(&json!({"error": "nope"})).unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
    }
}
