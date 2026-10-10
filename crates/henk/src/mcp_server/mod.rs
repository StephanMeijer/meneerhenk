//! Henk's own MCP server at `/mcp` (#247, #248, #249): an agent can start
//! a review, plan or address run, follow it, cancel it, and read review
//! quality and health, over the Model Context Protocol's Streamable HTTP
//! transport. Every tool is a thin call into what the dashboard's API does
//! ([`crate::dashboard::api`]); starting goes through the event bus like
//! any webhook, so the allowlist and every refusal apply.
//!
//! Each client has a named bearer token from the environment, `read` or
//! `write` (`[mcp_server]`). A request without a valid one is turned away
//! before any MCP handling; a read token may not start or cancel. What a
//! client starts is asked by `mcp:<token name>` on the record.

#[cfg(test)]
mod tests;
mod tools;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use henk_events::EventBus;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use secrecy::{ExposeSecret as _, SecretString};
use subtle::ConstantTimeEq as _;
use tracing::warn;

pub(crate) use tools::HenkMcp;
#[cfg(test)]
pub(crate) use tools::tool_names;

use crate::app::{App, env_var};
use crate::config::McpScope;
use crate::coordinator::Coordinator;

/// The largest request body `/mcp` takes: tool arguments are small.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Who called, as the auth layer found them: the token's name and scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Caller {
    /// The token's name.
    pub(crate) name: String,
    /// What it may do.
    pub(crate) scope: McpScope,
}

impl Caller {
    /// Who asked, on the record.
    pub(crate) fn requester(&self) -> String {
        format!("mcp:{}", self.name)
    }
}

/// A configured token whose variable is set.
pub(crate) struct Token {
    caller: Caller,
    secret: SecretString,
}

impl Token {
    /// A token named `name` with `scope`, whose secret is `secret`.
    pub(crate) fn new(name: &str, scope: McpScope, secret: SecretString) -> Self {
        Self {
            caller: Caller {
                name: name.to_owned(),
                scope,
            },
            secret,
        }
    }
}

/// What the auth layer checks against.
#[derive(Clone)]
struct Gate {
    enabled: bool,
    tokens: Arc<Vec<Token>>,
}

/// The tokens of `[mcp_server]` whose variables are set. A token whose
/// variable is missing cannot be used, and is said so once.
pub(crate) fn tokens_from_env(app: &App) -> Vec<Token> {
    let Some(config) = &app.settings.mcp_server else {
        return Vec::new();
    };
    config
        .tokens
        .iter()
        .filter_map(|token| match env_var(&token.env) {
            Some(secret) if !secret.is_empty() => Some(Token::new(
                &token.name,
                token.scope,
                SecretString::from(secret),
            )),
            _ => {
                warn!(token = %token.name, env = %token.env, "an MCP token's variable is not set; that client cannot connect");
                None
            }
        })
        .collect()
}

/// The hosts `/mcp` answers for: the public base URL's and loopback. A
/// request naming another host is refused, against DNS rebinding.
fn allowed_hosts(app: &App) -> Vec<String> {
    let mut hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    if let Ok(url) = reqwest::Url::parse(&app.settings.server.public_base_url)
        && let Some(host) = url.host_str()
    {
        hosts.push(host.to_owned());
    }
    hosts
}

/// `/mcp`: the MCP server behind `tokens` (from [`tokens_from_env`] in
/// `henk serve`). With `[mcp_server]` absent or off, or no token, it
/// answers 503.
pub(crate) fn routes(
    app: Arc<App>,
    coordinator: Arc<Coordinator>,
    bus: Arc<EventBus>,
    listeners: Vec<&'static str>,
    tokens: Vec<Token>,
) -> Router {
    let gate = Gate {
        enabled: app.settings.mcp_server.as_ref().is_some_and(|c| c.enabled),
        tokens: Arc::new(tokens),
    };
    let config = StreamableHttpServerConfig::default()
        // Stateless: each request stands alone, so no session table is kept.
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(allowed_hosts(&app))
        .with_max_request_body_bytes(MAX_BODY_BYTES);
    let server = HenkMcp::new(app, coordinator, bus, listeners);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .nest_service("/mcp", service)
        // Only on /mcp: a plain layer would also guard the fallback that
        // other paths reach once this router is merged.
        .route_layer(middleware::from_fn_with_state(gate, guard))
}

/// The bearer token of a request, or empty.
fn bearer(request: &Request) -> &str {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
}

/// Turns away a request without a valid token before any MCP handling,
/// and tells the tools who called. Every token is compared, in constant
/// time, so the time taken does not say which one matched. A secret that
/// matches more than one token is refused: its scope would be a guess.
async fn guard(State(gate): State<Gate>, mut request: Request, next: Next) -> Response {
    if !gate.enabled || gate.tokens.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the MCP server is not configured",
        )
            .into_response();
    }
    let given = bearer(&request).as_bytes();
    let mut found = Vec::new();
    for token in gate.tokens.iter() {
        if bool::from(given.ct_eq(token.secret.expose_secret().as_bytes())) {
            found.push(token.caller.clone());
        }
    }
    let [caller] = found.as_slice() else {
        let mut refused = (StatusCode::UNAUTHORIZED, "bad token").into_response();
        refused
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return refused;
    };
    request.extensions_mut().insert(caller.clone());
    next.run(request).await
}
