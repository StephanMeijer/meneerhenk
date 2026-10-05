//! How an external MCP server is reached.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use crate::error::McpError;

/// One external MCP server, as configured in `henk.toml`.
///
/// Exactly one of `command` (stdio child process) or `url` (streamable HTTP)
/// must be set; [`McpServerConfig::transport`] checks that.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// The executable of a stdio server.
    #[serde(default)]
    pub command: Option<String>,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables set to literal values. Never a secret.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Environment variables copied from Henk's own environment, by name.
    /// This is how tokens reach the server (§8.4): Henk's process holds
    /// them, the model never sees them.
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// The endpoint of a streamable HTTP server.
    #[serde(default)]
    pub url: Option<String>,
    /// Name of the environment variable holding the bearer token for `url`.
    #[serde(default)]
    pub bearer_env: Option<String>,
    /// Per-call timeout in seconds. Default 60.
    #[serde(default = "default_call_timeout_secs")]
    pub call_timeout_secs: u64,
}

fn default_call_timeout_secs() -> u64 {
    60
}

impl McpServerConfig {
    /// Per-call timeout.
    #[must_use]
    pub fn call_timeout(&self) -> Duration {
        Duration::from_secs(self.call_timeout_secs)
    }

    /// The transport this configuration describes.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::InvalidConfig`] unless exactly one of `command`
    /// and `url` is set, or when HTTP-only or stdio-only fields are mixed.
    pub fn transport(&self, alias: &str) -> Result<McpTransport, McpError> {
        let invalid = |detail: &str| McpError::InvalidConfig {
            alias: alias.to_owned(),
            detail: detail.to_owned(),
        };
        match (&self.command, &self.url) {
            (Some(command), None) => {
                if self.bearer_env.is_some() {
                    return Err(invalid("bearer_env only applies to url servers"));
                }
                Ok(McpTransport::Stdio {
                    command: command.clone(),
                    args: self.args.clone(),
                    env: self.env.clone(),
                    pass_env: self.pass_env.clone(),
                })
            }
            (None, Some(url)) => {
                if !self.args.is_empty() || !self.env.is_empty() || !self.pass_env.is_empty() {
                    return Err(invalid(
                        "args, env and pass_env only apply to command servers",
                    ));
                }
                Ok(McpTransport::Http {
                    url: url.clone(),
                    bearer_env: self.bearer_env.clone(),
                })
            }
            (Some(_), Some(_)) => Err(invalid("set either command or url, not both")),
            (None, None) => Err(invalid("set command (stdio) or url (http)")),
        }
    }
}

/// Transport to an external MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// A child process speaking MCP over stdio.
    Stdio {
        /// The executable.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// Literal environment variables.
        env: BTreeMap<String, String>,
        /// Environment variables forwarded from Henk's environment.
        pass_env: Vec<String>,
    },
    /// A remote server over streamable HTTP.
    Http {
        /// The MCP endpoint URL.
        url: String,
        /// Name of the environment variable holding the bearer token.
        bearer_env: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parses_stdio_and_http_variants() {
        let stdio: McpServerConfig = toml::from_str(
            r#"
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_TOOLSETS = "repos,pull_requests" }
pass_env = ["GITHUB_APP_PRIVATE_KEY_PATH"]
"#,
        )
        .unwrap();
        assert!(matches!(
            stdio.transport("github").unwrap(),
            McpTransport::Stdio { ref command, .. } if command == "github-mcp-server"
        ));
        assert_eq!(stdio.call_timeout(), Duration::from_mins(1));

        let http: McpServerConfig = toml::from_str(
            r#"
url = "https://api.fastmail.com/mcp"
bearer_env = "FASTMAIL_TOKEN"
call_timeout_secs = 30
"#,
        )
        .unwrap();
        assert!(matches!(
            http.transport("mail").unwrap(),
            McpTransport::Http { .. }
        ));
        assert_eq!(http.call_timeout(), Duration::from_secs(30));
    }

    #[test]
    fn rejects_unknown_fields_and_mixed_transports() {
        let result: Result<McpServerConfig, _> = toml::from_str("command = \"x\"\nbogus = 1\n");
        assert!(result.is_err());

        let both: McpServerConfig =
            toml::from_str("command = \"x\"\nurl = \"https://y\"\n").unwrap();
        assert!(matches!(
            both.transport("a"),
            Err(McpError::InvalidConfig { .. })
        ));
        let neither: McpServerConfig = toml::from_str("call_timeout_secs = 1\n").unwrap();
        assert!(matches!(
            neither.transport("a"),
            Err(McpError::InvalidConfig { .. })
        ));
        let mixed: McpServerConfig =
            toml::from_str("url = \"https://y\"\nargs = [\"a\"]\n").unwrap();
        assert!(matches!(
            mixed.transport("a"),
            Err(McpError::InvalidConfig { .. })
        ));
    }
}
