//! Everything a running Henk shares: settings, store, models and writers.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use henk_llm::{ModelClient, client_for};
use henk_mcp::{McpServerConfig, McpSession, RmcpSession};
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth, GitHubWriter};
use henk_platform::gitlab::GitLabWriter;
use henk_platform::{IssueWriter, PlatformWriter};
use henk_store::RunStore;
use tokio_util::sync::CancellationToken;

use crate::config::Settings;

/// The shared application state.
pub struct App {
    /// Validated configuration.
    pub settings: Settings,
    /// Run records.
    pub store: Arc<RunStore>,
    /// Model clients by configured id.
    pub models: BTreeMap<String, Arc<dyn ModelClient>>,
    /// The GitHub writer, when GitHub is configured.
    pub github: Option<Arc<GitHubWriter>>,
    /// The GitLab writer over its write-mode MCP session, when configured.
    pub gitlab: Option<Arc<GitLabWriter>>,
    /// Cancelled when Henk is told to stop (Ctrl-C, SIGTERM). Every review
    /// and plan runs under a child of it, so a run can tell being stopped
    /// from being superseded.
    pub shutdown: CancellationToken,
    /// Tests: the writer `writer` returns for every platform.
    #[cfg(test)]
    pub test_writer: Option<Arc<dyn PlatformWriter>>,
    /// Tests: the session `read_session` returns instead of starting one.
    #[cfg(test)]
    pub test_session: Option<Arc<dyn McpSession>>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("models", &self.models.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Reads an environment variable, treating empty as unset.
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl App {
    /// Builds the application: opens the store, builds every configured
    /// model client and the GitHub writer. Secrets are read from the
    /// environment here and nowhere else.
    ///
    /// # Errors
    ///
    /// Returns an error naming the first missing secret or unusable setting.
    pub async fn build(settings: Settings, database: Option<&Path>) -> anyhow::Result<Self> {
        henk_llm::ensure_tls_provider();
        let store = match database {
            Some(path) => {
                RunStore::open(path).with_context(|| format!("opening {}", path.display()))?
            }
            None => RunStore::open(Path::new(&settings.server.database_path))
                .with_context(|| format!("opening {}", settings.server.database_path))?,
        };

        let mut models: BTreeMap<String, Arc<dyn ModelClient>> = BTreeMap::new();
        for (id, model) in &settings.models {
            let config = model
                .to_client_config(env_var)
                .map_err(|var| anyhow!("model {id}: environment variable {var} is not set"))?;
            let client = client_for(config).with_context(|| format!("model {id}"))?;
            models.insert(id.clone(), client);
        }

        let github = match &settings.github {
            Some(github) => {
                let key_path = env_var(&github.private_key_path_env).ok_or_else(|| {
                    anyhow!(
                        "environment variable {} is not set",
                        github.private_key_path_env
                    )
                })?;
                let pem = std::fs::read_to_string(&key_path)
                    .with_context(|| format!("reading {key_path}"))?;
                let credentials =
                    AppCredentials::from_pem(github.app_id, github.installation_id, &pem)
                        .context("GitHub App private key")?;
                let api = GitHubApi::new(&github.api_base, GitHubAuth::app(credentials))
                    .context("GitHub client")?;
                Some(Arc::new(GitHubWriter::new(api, github.bot_login.clone())))
            }
            None => None,
        };

        let gitlab = match &settings.gitlab {
            Some(gitlab) => {
                let config = settings.mcp.get(&gitlab.write_mcp_server).ok_or_else(|| {
                    anyhow!("MCP server {:?} is not configured", gitlab.write_mcp_server)
                })?;
                let session = RmcpSession::connect(&gitlab.write_mcp_server, config, env_var)
                    .await
                    .with_context(|| format!("MCP server {}", gitlab.write_mcp_server))?;
                Some(Arc::new(GitLabWriter::new(
                    Arc::new(session),
                    gitlab.username.clone(),
                )))
            }
            None => None,
        };

        Ok(Self {
            settings,
            store: Arc::new(store),
            models,
            github,
            gitlab,
            shutdown: CancellationToken::new(),
            #[cfg(test)]
            test_writer: None,
            #[cfg(test)]
            test_session: None,
        })
    }

    /// The model client behind a configured id.
    ///
    /// # Errors
    ///
    /// Returns an error when the id is not configured.
    pub fn model(&self, id: &str) -> anyhow::Result<Arc<dyn ModelClient>> {
        self.models
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("model {id:?} is not configured"))
    }

    /// The configuration of an MCP server by alias.
    ///
    /// # Errors
    ///
    /// Returns an error when the alias is not configured.
    pub fn mcp_config(&self, alias: &str) -> anyhow::Result<&McpServerConfig> {
        self.settings
            .mcp
            .get(alias)
            .ok_or_else(|| anyhow!("MCP server {alias:?} is not configured"))
    }

    /// Starts an MCP session by alias.
    ///
    /// # Errors
    ///
    /// Returns an error when the alias is unknown or the server does not start.
    pub async fn connect_mcp(&self, alias: &str) -> anyhow::Result<RmcpSession> {
        let config = self.mcp_config(alias)?;
        RmcpSession::connect(alias, config, env_var)
            .await
            .with_context(|| format!("MCP server {alias}"))
    }

    /// The read-only MCP session for a platform: its configured server,
    /// started now.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform or its server is not configured,
    /// or the server does not start.
    pub async fn read_session(
        &self,
        platform: henk_domain::allowlist::Platform,
    ) -> anyhow::Result<Arc<dyn McpSession>> {
        #[cfg(test)]
        if let Some(session) = &self.test_session {
            return Ok(Arc::clone(session));
        }
        let alias = self.read_mcp_alias(platform)?;
        Ok(Arc::new(self.connect_mcp(alias).await?))
    }

    /// The writer for a platform.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured.
    pub fn writer(
        &self,
        platform: henk_domain::allowlist::Platform,
    ) -> anyhow::Result<Arc<dyn PlatformWriter>> {
        #[cfg(test)]
        if let Some(writer) = &self.test_writer {
            return Ok(Arc::clone(writer));
        }
        match platform {
            henk_domain::allowlist::Platform::GitHub => self
                .github
                .clone()
                .map(|w| w as Arc<dyn PlatformWriter>)
                .ok_or_else(|| anyhow!("GitHub is not configured")),
            henk_domain::allowlist::Platform::GitLab => self
                .gitlab
                .clone()
                .map(|w| w as Arc<dyn PlatformWriter>)
                .ok_or_else(|| anyhow!("GitLab is not configured")),
        }
    }

    /// The issue writer for a platform.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured.
    pub fn issue_writer(
        &self,
        platform: henk_domain::allowlist::Platform,
    ) -> anyhow::Result<Arc<dyn IssueWriter>> {
        match platform {
            henk_domain::allowlist::Platform::GitHub => self
                .github
                .clone()
                .map(|w| w as Arc<dyn IssueWriter>)
                .ok_or_else(|| anyhow!("GitHub is not configured")),
            henk_domain::allowlist::Platform::GitLab => self
                .gitlab
                .clone()
                .map(|w| w as Arc<dyn IssueWriter>)
                .ok_or_else(|| anyhow!("GitLab is not configured")),
        }
    }

    /// Alias of the read-only MCP server for a platform.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured.
    pub fn read_mcp_alias(
        &self,
        platform: henk_domain::allowlist::Platform,
    ) -> anyhow::Result<&str> {
        match platform {
            henk_domain::allowlist::Platform::GitHub => self
                .settings
                .github
                .as_ref()
                .map(|g| g.mcp_server.as_str())
                .ok_or_else(|| anyhow!("GitHub is not configured")),
            henk_domain::allowlist::Platform::GitLab => self
                .settings
                .gitlab
                .as_ref()
                .map(|g| g.mcp_server.as_str())
                .ok_or_else(|| anyhow!("GitLab is not configured")),
        }
    }
}
