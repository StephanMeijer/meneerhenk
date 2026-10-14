//! Everything a running Henk shares: settings, store, models and writers.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use henk_domain::allowlist::{Platform, RepoRef};
use henk_llm::{ModelClient, client_with};
use henk_mcp::{McpServerConfig, McpSession, RmcpSession};
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth, GitHubWriter, Installations};
use henk_platform::gitlab::{GitLabRest, GitLabWriter};
use henk_platform::push::PushWriter;
use henk_platform::{CiReader, IssueWriter, PlatformWriter};
use henk_store::{PgStore, RunStore, SqliteStore};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::{DatabaseConfig, GitHubConfig, Settings};
use crate::liveness::LiveRuns;
use crate::workspace::Backends;
use crate::workspace::WorkspaceProvider;

/// The shared application state.
pub struct App {
    /// Validated configuration.
    pub settings: Settings,
    /// Run records.
    pub store: Arc<dyn RunStore>,
    /// Model clients by configured id.
    pub models: BTreeMap<String, Arc<dyn ModelClient>>,
    /// GitHub's writers, one per repository, when GitHub is configured.
    pub github: Option<GitHubWriters>,
    /// The GitLab writer over its write-mode MCP session, when configured.
    pub gitlab: Option<Arc<GitLabWriter>>,
    /// Cancelled when Henk is told to stop (Ctrl-C, SIGTERM). Every review
    /// and plan runs under a child of it, so a run can tell being stopped
    /// from being superseded.
    pub shutdown: CancellationToken,
    /// The runs this process is working on, which the reaper leaves alone.
    pub live_runs: LiveRuns,
    /// What changes in the store, as it happens; the dashboard's streams
    /// follow it (#202).
    pub feed: crate::live::Feed,
    /// The runs a person may cancel from the dashboard, and who did (#69).
    pub cancels: crate::cancel::Cancels,
    /// Opens the workspaces runs work in, on the backend each profile
    /// names; tests swap in another.
    pub workspace_provider: Arc<dyn WorkspaceProvider>,
    /// The commits Henk pushed, whose events start no review (#284).
    pub own_pushes: crate::push::OwnPushes,
    /// Tests: the writer `writer` returns for every platform.
    #[cfg(test)]
    pub test_writer: Option<Arc<dyn PlatformWriter>>,
    /// Tests: the session `read_session` returns instead of starting one.
    #[cfg(test)]
    pub test_session: Option<Arc<dyn McpSession>>,
    /// Tests: what `push_writer` returns for every platform.
    #[cfg(test)]
    pub test_push_writer: Option<Arc<dyn PushWriter>>,
    /// Tests: the tracker `issue_writer` returns for every platform.
    #[cfg(test)]
    pub test_issue_writer: Option<Arc<dyn IssueWriter>>,
    /// Tests: what `ci_reader` returns for every platform.
    #[cfg(test)]
    pub test_ci_reader: Option<Arc<dyn CiReader>>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("models", &self.models.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Makes the GitHub writer for each repository. Every writer it makes acts
/// for the App installation that reaches its repository, so a run's
/// comments, checks, clones and pushes all go out with that installation's
/// token, never another account's (#378).
#[derive(Debug)]
pub struct GitHubWriters {
    api: GitHubApi,
    bot_login: String,
}

impl GitHubWriters {
    /// Writers over `api`, unscoped, posting as `bot_login`.
    #[must_use]
    pub fn new(api: GitHubApi, bot_login: impl Into<String>) -> Self {
        Self {
            api,
            bot_login: bot_login.into(),
        }
    }

    /// The writer for `repo`'s installation.
    #[must_use]
    pub fn for_repository(&self, repo: &RepoRef) -> Arc<GitHubWriter> {
        Arc::new(GitHubWriter::new(
            self.api.for_repository(repo),
            self.bot_login.clone(),
        ))
    }

    /// The unscoped client: App calls, and the installations to remember a
    /// webhook's installation in.
    #[must_use]
    pub const fn api(&self) -> &GitHubApi {
        &self.api
    }
}

/// The unscoped GitHub App client `github` configures, its key read from
/// the file its environment variable names.
///
/// # Errors
///
/// Returns an error when the variable is unset, or the key is unreadable.
pub fn github_api(github: &GitHubConfig) -> anyhow::Result<GitHubApi> {
    let key_path = env_var(&github.private_key_path_env).ok_or_else(|| {
        anyhow!(
            "environment variable {} is not set",
            github.private_key_path_env
        )
    })?;
    let pem = std::fs::read_to_string(&key_path).with_context(|| format!("reading {key_path}"))?;
    let credentials =
        AppCredentials::from_pem(github.app_id, &pem).context("GitHub App private key")?;
    let installations = Installations::new(credentials, github.ceiling());
    GitHubApi::new(&github.api_base, GitHubAuth::app(Arc::new(installations)))
        .context("GitHub client")
}

/// `config` for a session that reads `repo`, signed in as `installation` of
/// App `app_id`: the GitHub MCP server mints that installation's tokens
/// itself, so it reaches that account's repositories and no other's.
#[must_use]
pub fn github_session_config(
    config: &McpServerConfig,
    app_id: u64,
    installation: u64,
) -> McpServerConfig {
    let mut config = config.clone();
    let [app_key, installation_key] = crate::config::MCP_INSTALLATION_ENV;
    config.env.insert(app_key.to_owned(), app_id.to_string());
    config
        .env
        .insert(installation_key.to_owned(), installation.to_string());
    config
}

/// Reads an environment variable, treating empty as unset.
pub fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Opens the configured run store. A `PostgreSQL` URL holds a password, so
/// it is read from its environment variable here (§8.4) and never shown.
///
/// # Errors
///
/// Returns an error when the variable is unset or the store cannot be opened.
pub async fn open_store(database: &DatabaseConfig) -> anyhow::Result<Arc<dyn RunStore>> {
    match database {
        DatabaseConfig::Sqlite { path } => Ok(Arc::new(
            SqliteStore::open(Path::new(path)).with_context(|| format!("opening {path}"))?,
        )),
        DatabaseConfig::Postgres { url_env } => {
            let url = env_var(url_env)
                .ok_or_else(|| anyhow!("environment variable {url_env} is not set"))?;
            let place = henk_store::describe_url(&url)?;
            Ok(Arc::new(PgStore::connect(&url).await.with_context(
                || format!("connecting to PostgreSQL at {place}"),
            )?))
        }
    }
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
        let store: Arc<dyn RunStore> = match database {
            Some(path) => Arc::new(
                SqliteStore::open(path).with_context(|| format!("opening {}", path.display()))?,
            ),
            None => open_store(&settings.database).await?,
        };
        let feed = crate::live::Feed::default();
        let store: Arc<dyn RunStore> = Arc::new(crate::live::Announcing::new(store, feed.clone()));

        // One HTTP client per endpoint, shared by its models (#368).
        let mut endpoints: BTreeMap<&str, reqwest::Client> = BTreeMap::new();
        for id in settings.endpoints.keys() {
            let http =
                henk_llm::build_endpoint_client().with_context(|| format!("endpoint {id}"))?;
            endpoints.insert(id.as_str(), http);
        }
        let mut models: BTreeMap<String, Arc<dyn ModelClient>> = BTreeMap::new();
        for (id, model) in &settings.models {
            let config = settings
                .client_config(id, env_var)
                .map_err(|why| anyhow!("model {id} on endpoint {}: {why}", model.endpoint))?;
            let http = endpoints
                .get(model.endpoint.as_str())
                .cloned()
                .ok_or_else(|| {
                    anyhow!("model {id}: endpoint {} is not configured", model.endpoint)
                })?;
            let client = client_with(config, http).with_context(|| format!("model {id}"))?;
            models.insert(id.clone(), client);
        }

        let github = match &settings.github {
            Some(github) => Some(GitHubWriters::new(
                github_api(github)?,
                github.bot_login.clone(),
            )),
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
                let writer = GitLabWriter::new(Arc::new(session), gitlab.username.clone());
                // Henk pushes the executor's fixes with the token (§3.5) and
                // reads what the MCP server does not give; it goes to git's environment and
                // a request header, never to argv, a log or a model.
                let writer = if let Some(token) = env_var(&gitlab.token_env) {
                    writer.with_rest(
                        GitLabRest::new(&gitlab.api_url, SecretString::from(token))
                            .context("GitLab client")?,
                    )
                } else {
                    warn!(
                        variable = %gitlab.token_env,
                        "not set; reviews on GitLab cannot push"
                    );
                    writer
                };
                Some(Arc::new(writer))
            }
            None => None,
        };

        // The sandbox host's key is read now, so a missing or broken one
        // stops Henk at start instead of failing the first review on it.
        let ssh = crate::workspace::ssh_pool(&settings)?;
        let kubernetes = crate::workspace::kubernetes_provider(&settings).await?;

        Ok(Self {
            settings,
            store,
            models,
            github,
            gitlab,
            shutdown: CancellationToken::new(),
            live_runs: LiveRuns::default(),
            feed,
            cancels: crate::cancel::Cancels::default(),
            workspace_provider: Arc::new(Backends::new(ssh, kubernetes)),
            own_pushes: crate::push::OwnPushes::default(),
            #[cfg(test)]
            test_writer: None,
            #[cfg(test)]
            test_session: None,
            #[cfg(test)]
            test_push_writer: None,
            #[cfg(test)]
            test_issue_writer: None,
            #[cfg(test)]
            test_ci_reader: None,
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

    /// The read-only MCP session for reading `repo`: its platform's
    /// configured server, started now. On GitHub it signs in as the
    /// installation that reaches `repo`.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform or its server is not configured,
    /// no installation reaches `repo`, or the server does not start.
    pub async fn read_session(&self, repo: &RepoRef) -> anyhow::Result<Arc<dyn McpSession>> {
        #[cfg(test)]
        if let Some(session) = &self.test_session {
            return Ok(Arc::clone(session));
        }
        let alias = self.read_mcp_alias(repo.platform())?;
        let config = self.mcp_config(alias)?;
        let config = match (repo.platform(), &self.github) {
            (Platform::GitHub, Some(github)) => {
                let api = github.api().for_repository(repo);
                let installation = api
                    .installation()
                    .await
                    .with_context(|| format!("GitHub installation for {}", repo.path()))?;
                let app_id = api
                    .installations()
                    .map(|installations| installations.app_id())
                    .ok_or_else(|| anyhow!("GitHub is not configured as an App"))?;
                github_session_config(config, app_id, installation)
            }
            _ => config.clone(),
        };
        let session = RmcpSession::connect(alias, &config, env_var)
            .await
            .with_context(|| format!("MCP server {alias}"))?;
        Ok(Arc::new(session))
    }

    /// The writer for `repo`: on GitHub, its installation's.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured.
    pub fn writer(&self, repo: &RepoRef) -> anyhow::Result<Arc<dyn PlatformWriter>> {
        #[cfg(test)]
        if let Some(writer) = &self.test_writer {
            return Ok(Arc::clone(writer));
        }
        match repo.platform() {
            henk_domain::allowlist::Platform::GitHub => self
                .github
                .as_ref()
                .map(|w| w.for_repository(repo) as Arc<dyn PlatformWriter>)
                .ok_or_else(|| anyhow!("GitHub is not configured")),
            henk_domain::allowlist::Platform::GitLab => self
                .gitlab
                .clone()
                .map(|w| w as Arc<dyn PlatformWriter>)
                .ok_or_else(|| anyhow!("GitLab is not configured")),
        }
    }

    /// What Henk reads and writes to push to `repo`.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured, or GitLab is
    /// without its token.
    pub fn push_writer(&self, repo: &RepoRef) -> anyhow::Result<Arc<dyn PushWriter>> {
        #[cfg(test)]
        if let Some(writer) = &self.test_push_writer {
            return Ok(Arc::clone(writer));
        }
        match repo.platform() {
            henk_domain::allowlist::Platform::GitHub => self
                .github
                .as_ref()
                .map(|w| w.for_repository(repo) as Arc<dyn PushWriter>)
                .ok_or_else(|| anyhow!("GitHub is not configured")),
            henk_domain::allowlist::Platform::GitLab => match &self.gitlab {
                Some(writer) if writer.can_push() => Ok(Arc::clone(writer) as Arc<dyn PushWriter>),
                Some(_) => Err(anyhow!(
                    "pushing to GitLab needs the token in ${}",
                    self.settings
                        .gitlab
                        .as_ref()
                        .map_or("GITLAB_PERSONAL_ACCESS_TOKEN", |g| g.token_env.as_str())
                )),
                None => Err(anyhow!("GitLab is not configured")),
            },
        }
    }

    /// The issue writer for `repo`.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured.
    pub fn issue_writer(&self, repo: &RepoRef) -> anyhow::Result<Arc<dyn IssueWriter>> {
        #[cfg(test)]
        if let Some(writer) = &self.test_issue_writer {
            return Ok(Arc::clone(writer));
        }
        match repo.platform() {
            henk_domain::allowlist::Platform::GitHub => self
                .github
                .as_ref()
                .map(|w| w.for_repository(repo) as Arc<dyn IssueWriter>)
                .ok_or_else(|| anyhow!("GitHub is not configured")),
            henk_domain::allowlist::Platform::GitLab => self
                .gitlab
                .clone()
                .map(|w| w as Arc<dyn IssueWriter>)
                .ok_or_else(|| anyhow!("GitLab is not configured")),
        }
    }

    /// What reads the CI of `repo`'s pull requests (#420). On GitLab
    /// without its token every read says so.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform is not configured.
    pub fn ci_reader(&self, repo: &RepoRef) -> anyhow::Result<Arc<dyn CiReader>> {
        #[cfg(test)]
        if let Some(reader) = &self.test_ci_reader {
            return Ok(Arc::clone(reader));
        }
        match repo.platform() {
            henk_domain::allowlist::Platform::GitHub => self
                .github
                .as_ref()
                .map(|w| w.for_repository(repo) as Arc<dyn CiReader>)
                .ok_or_else(|| anyhow!("GitHub is not configured")),
            henk_domain::allowlist::Platform::GitLab => self
                .gitlab
                .clone()
                .map(|w| w as Arc<dyn CiReader>)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::github_session_config;

    #[test]
    fn the_github_mcp_server_signs_in_as_the_repositorys_installation() {
        let config: henk_mcp::McpServerConfig = toml::from_str(
            "command = \"github-mcp-server\"\nenv = { GITHUB_READ_ONLY = \"1\" }\npass_env = [\"GITHUB_APP_PRIVATE_KEY_PATH\"]\n",
        )
        .unwrap();
        let session = github_session_config(&config, 12, 222);
        assert_eq!(session.env["GITHUB_APP_ID"], "12");
        assert_eq!(session.env["GITHUB_APP_INSTALLATION_ID"], "222");
        assert_eq!(session.env["GITHUB_READ_ONLY"], "1", "the rest is kept");
        assert_eq!(session.pass_env, config.pass_env);
        assert!(
            !config.env.contains_key("GITHUB_APP_INSTALLATION_ID"),
            "the configured server is left as it is"
        );
    }
}
