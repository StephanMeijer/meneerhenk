//! Listing the models an endpoint serves, for setup and `henk doctor`.

use secrecy::ExposeSecret as _;
use serde_json::Value;

use crate::client::{ModelConfig, Provider, TokenSource};
use crate::error::{LlmError, truncate_body};
use crate::http::{build_endpoint_client, get_json, prepared, with_retry};

/// Lists the model ids the endpoint of `config` serves. The configured model
/// name itself is ignored; only the base URL, provider and key are used.
///
/// With a sign-in in `config` the bearer comes from it, a refused one is
/// renewed and the call made once more, and the models the plan does not
/// list (`visibility` other than `list`) are left out (#620).
///
/// # Errors
///
/// Returns [`LlmError::InvalidConfig`] when no HTTP client can be built, the
/// usual HTTP errors from the endpoint, and [`LlmError::Decode`] when the
/// answer has no `data` array of objects with an `id`.
pub async fn list_models(config: &ModelConfig) -> Result<Vec<String>, LlmError> {
    let http = build_endpoint_client()?;
    let url = match config.provider {
        Provider::OpenAi => format!("{}/models", config.base_url()),
        Provider::Anthropic => format!("{}/v1/models", config.base_url()),
    };
    with_retry(config.retry, |_| async {
        if let Some(source) = &config.token_source {
            return list_on_plan(source.as_ref(), &http, &url, config).await;
        }
        let builder = match config.provider {
            Provider::OpenAi => {
                prepared(http.get(&url), config).bearer_auth(config.api_key.expose_secret())
            }
            Provider::Anthropic => prepared(http.get(&url), config)
                .header("x-api-key", config.api_key.expose_secret())
                .header("anthropic-version", crate::anthropic::API_VERSION),
        };
        let response = get_json(builder).await?;
        decode(&response)
    })
    .await
}

/// One attempt at the list with a sign-in's bearer; a refusal renews the
/// token and tries once more.
async fn list_on_plan(
    source: &dyn TokenSource,
    http: &reqwest::Client,
    url: &str,
    config: &ModelConfig,
) -> Result<Vec<String>, LlmError> {
    let mut renewed = false;
    loop {
        let token = source.bearer().await?;
        let builder = prepared(http.get(url), config).bearer_auth(token.expose_secret());
        match get_json(builder).await {
            Err(LlmError::Unauthorized { status: 401, .. }) if !renewed => {
                source.rejected(&token).await;
                renewed = true;
            }
            Ok(response) => return decode(&response),
            Err(error) => return Err(error),
        }
    }
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
        // A plan lists the models it offers with `visibility: "list"`; an
        // entry with no such field (every other endpoint) is kept.
        .filter(|entry| {
            entry
                .get("visibility")
                .and_then(Value::as_str)
                .is_none_or(|visibility| visibility == "list")
        })
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
    fn models_a_plan_does_not_list_are_left_out() {
        let ids = decode(&json!({"data": [
            {"id": "shown", "visibility": "list"},
            {"id": "hidden", "visibility": "hide"},
            {"id": "plain"},
        ]}))
        .unwrap();
        assert_eq!(ids, vec!["plain", "shown"]);
    }

    #[test]
    fn missing_data_is_a_decode_error_with_body() {
        let error = decode(&json!({"error": "nope"})).unwrap_err();
        assert!(error.to_string().contains("nope"), "{error}");
    }
}
