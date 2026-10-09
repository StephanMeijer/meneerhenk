//! One connection to one MCP server.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ResourceContents, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt as _, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ServiceError, ServiceExt as _};
use serde_json::Value;
use tokio::io::AsyncBufReadExt as _;
use tokio::process::Command;
use tracing::{debug, info, instrument, warn};

use crate::config::{McpServerConfig, McpTransport};
use crate::error::McpError;

/// A tool as the server declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    /// The server's name for it.
    pub name: String,
    /// What it does.
    pub description: String,
    /// JSON Schema of its arguments.
    pub input_schema: Value,
}

impl From<Tool> for ToolInfo {
    fn from(tool: Tool) -> Self {
        Self {
            name: tool.name.into_owned(),
            description: tool
                .description
                .map(std::borrow::Cow::into_owned)
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
        }
    }
}

/// What a tool call produced, flattened to text for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Text content, one block per line. Non-text blocks are described.
    pub text: String,
    /// Whether the server flagged the result as an error.
    pub is_error: bool,
    /// Structured content, when the server provided it.
    pub structured: Option<Value>,
}

impl From<CallToolResult> for ToolOutcome {
    fn from(result: CallToolResult) -> Self {
        let mut lines = Vec::new();
        for block in &result.content {
            match block {
                ContentBlock::Text(text) => lines.push(text.text.clone()),
                ContentBlock::Image(image) => lines.push(format!("[image {}]", image.mime_type)),
                ContentBlock::Audio(audio) => lines.push(format!("[audio {}]", audio.mime_type)),
                ContentBlock::Resource(embedded) => match &embedded.resource {
                    ResourceContents::TextResourceContents { uri, text, .. } => {
                        lines.push(format!("[{uri}]\n{text}"));
                    }
                    ResourceContents::BlobResourceContents { uri, .. } => {
                        lines.push(format!("[binary resource {uri}]"));
                    }
                    _ => lines.push("[resource]".to_owned()),
                },
                ContentBlock::ResourceLink(resource) => {
                    lines.push(format!("[resource {}]", resource.uri));
                }
                _ => lines.push("[unsupported content]".to_owned()),
            }
        }
        let mut text = lines.join("\n");
        if text.is_empty()
            && let Some(structured) = &result.structured_content
        {
            text = structured.to_string();
        }
        Self {
            text,
            is_error: result.is_error.unwrap_or(false),
            structured: result.structured_content,
        }
    }
}

/// One live MCP server connection.
#[async_trait::async_trait]
pub trait McpSession: Send + Sync {
    /// The configured alias, used as the prefix of model-facing tool names.
    fn alias(&self) -> &str;

    /// Every tool the server offers.
    async fn list_tools(&self) -> Result<Vec<ToolInfo>, McpError>;

    /// Calls one tool. `arguments` must be a JSON object.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutcome, McpError>;
}

/// An [`McpSession`] backed by rmcp.
pub struct RmcpSession {
    alias: String,
    service: RunningService<RoleClient, ()>,
    call_timeout: Duration,
}

impl std::fmt::Debug for RmcpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RmcpSession")
            .field("alias", &self.alias)
            .finish_non_exhaustive()
    }
}

/// Environment variables a child server always inherits, so that `npx`,
/// `docker` and friends keep working while everything else stays out.
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TERM",
    "TMPDIR",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
];

/// The whole environment a stdio child receives (#62): the [`INHERITED_ENV`]
/// names `lookup` knows, then every `pass_env` name, then the fixed `env`
/// map, which wins. Nothing else of Henk's environment comes through.
///
/// # Errors
///
/// Returns [`McpError::MissingEnv`] when `lookup` does not know a `pass_env`
/// name.
fn child_env(
    pass_env: &[String],
    env: &BTreeMap<String, String>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>, McpError> {
    let mut full_env: BTreeMap<String, String> = INHERITED_ENV
        .iter()
        .filter_map(|name| lookup(name).map(|value| ((*name).to_owned(), value)))
        .collect();
    for name in pass_env {
        let value = lookup(name).ok_or_else(|| McpError::MissingEnv(name.clone()))?;
        full_env.insert(name.clone(), value);
    }
    full_env.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
    Ok(full_env)
}

impl RmcpSession {
    /// Connects to a configured server. `lookup_env` resolves the names in
    /// `pass_env` and `bearer_env`; in production it is `std::env::var`.
    ///
    /// # Errors
    ///
    /// Returns [`McpError`] when the process cannot start, a named
    /// environment variable is missing, or the handshake fails.
    #[instrument(skip_all, fields(alias = %alias))]
    pub async fn connect(
        alias: &str,
        config: &McpServerConfig,
        lookup_env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, McpError> {
        let service = match config.transport(alias)? {
            McpTransport::Stdio {
                command,
                args,
                env,
                pass_env,
            } => {
                let full_env = child_env(&pass_env, &env, &lookup_env)?;
                let command = Command::new(&command).configure(|c| {
                    c.args(&args).env_clear().envs(&full_env).kill_on_drop(true);
                });
                let (process, stderr) = TokioChildProcess::builder(command)
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|source| McpError::Spawn {
                        alias: alias.to_owned(),
                        source,
                    })?;
                if let Some(stderr) = stderr {
                    forward_stderr(alias.to_owned(), stderr);
                }
                ().serve(process)
                    .await
                    .map_err(|error| McpError::Initialize {
                        alias: alias.to_owned(),
                        detail: error.to_string(),
                    })?
            }
            McpTransport::Http { url, bearer_env } => {
                let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url);
                if let Some(name) = &bearer_env {
                    let token =
                        lookup_env(name).ok_or_else(|| McpError::MissingEnv(name.clone()))?;
                    transport_config = transport_config.auth_header(token);
                }
                let transport = StreamableHttpClientTransport::from_config(transport_config);
                ().serve(transport)
                    .await
                    .map_err(|error| McpError::Initialize {
                        alias: alias.to_owned(),
                        detail: error.to_string(),
                    })?
            }
        };
        let server = service
            .peer_info()
            .and_then(|info| info.server_info.as_ref().map(|i| i.name.clone()));
        info!(
            server = server.as_deref().unwrap_or("?"),
            "MCP session established"
        );
        Ok(Self {
            alias: alias.to_owned(),
            service,
            call_timeout: config.call_timeout(),
        })
    }

    /// Wraps an already running client service, as the test fake does.
    #[must_use]
    pub fn from_service(
        alias: &str,
        service: RunningService<RoleClient, ()>,
        call_timeout: Duration,
    ) -> Self {
        Self {
            alias: alias.to_owned(),
            service,
            call_timeout,
        }
    }

    /// Closes the session and, for a child process, ends it.
    pub async fn close(self) {
        if let Err(error) = self.service.cancel().await {
            warn!(alias = %self.alias, %error, "MCP session did not shut down cleanly");
        }
    }

    fn service_error(&self, source: ServiceError) -> McpError {
        McpError::Service {
            alias: self.alias.clone(),
            source,
        }
    }
}

fn forward_stderr(alias: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            debug!(server = %alias, "{line}");
        }
    });
}

#[async_trait::async_trait]
impl McpSession for RmcpSession {
    fn alias(&self) -> &str {
        &self.alias
    }

    async fn list_tools(&self) -> Result<Vec<ToolInfo>, McpError> {
        let tools = tokio::time::timeout(self.call_timeout, self.service.list_all_tools())
            .await
            .map_err(|_| McpError::Timeout {
                alias: self.alias.clone(),
                tool: "tools/list".to_owned(),
                timeout: self.call_timeout,
            })?
            .map_err(|e| self.service_error(e))?;
        Ok(tools.into_iter().map(ToolInfo::from).collect())
    }

    #[instrument(skip_all, fields(alias = %self.alias, tool = %name))]
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolOutcome, McpError> {
        let arguments = match arguments {
            Value::Object(map) => map,
            Value::Null => serde_json::Map::new(),
            other => {
                return Err(McpError::InvalidConfig {
                    alias: self.alias.clone(),
                    detail: format!("tool arguments must be an object, got {other}"),
                });
            }
        };
        let params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        let result = tokio::time::timeout(self.call_timeout, self.service.call_tool(params))
            .await
            .map_err(|_| McpError::Timeout {
                alias: self.alias.clone(),
                tool: name.to_owned(),
                timeout: self.call_timeout,
            })?
            .map_err(|e| self.service_error(e))?;
        Ok(ToolOutcome::from(result))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A parent environment holding Henk's secrets next to the basics.
    fn parent(name: &str) -> Option<String> {
        match name {
            "PATH" => Some("/usr/bin".to_owned()),
            "HOME" => Some("/home/henk".to_owned()),
            "GITHUB_TOKEN" => Some("ghp_secret".to_owned()),
            "DATABASE_URL" => Some("postgres://henk:secret@db/henk".to_owned()),
            "GITLAB_PERSONAL_ACCESS_TOKEN" => Some("glpat-secret".to_owned()),
            _ => None,
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn only_inherited_names_come_through() {
        let env = child_env(&[], &BTreeMap::new(), parent).unwrap();
        assert_eq!(env, map(&[("HOME", "/home/henk"), ("PATH", "/usr/bin")]));
    }

    #[test]
    fn inherited_names_are_the_fixed_eight() {
        let every = |name: &str| Some(format!("value of {name}"));
        let env = child_env(&[], &BTreeMap::new(), every).unwrap();
        let names: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "HOME",
                "LANG",
                "LC_ALL",
                "PATH",
                "TERM",
                "TMPDIR",
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
            ]
        );
    }

    #[test]
    fn a_known_pass_env_name_is_included() {
        let pass_env = ["GITLAB_PERSONAL_ACCESS_TOKEN".to_owned()];
        let env = child_env(&pass_env, &BTreeMap::new(), parent).unwrap();
        assert_eq!(
            env,
            map(&[
                ("GITLAB_PERSONAL_ACCESS_TOKEN", "glpat-secret"),
                ("HOME", "/home/henk"),
                ("PATH", "/usr/bin"),
            ])
        );
    }

    #[test]
    fn an_unknown_pass_env_name_is_missing_env() {
        let error = child_env(&["NOT_SET".to_owned()], &BTreeMap::new(), parent).unwrap_err();
        assert!(matches!(error, McpError::MissingEnv(ref name) if name == "NOT_SET"));
    }

    #[test]
    fn fixed_env_overrides_inherited_and_adds_new_names() {
        let fixed = map(&[("HOME", "/srv/mcp"), ("GITHUB_TOOLSETS", "repos")]);
        let env = child_env(&[], &fixed, parent).unwrap();
        assert_eq!(
            env,
            map(&[
                ("GITHUB_TOOLSETS", "repos"),
                ("HOME", "/srv/mcp"),
                ("PATH", "/usr/bin"),
            ])
        );
    }
}
