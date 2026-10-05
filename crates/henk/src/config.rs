//! Configuration: identities, the allowlist, models, lanes and MCP servers.
//!
//! No credential lives here (§8.4). Every secret is named by the environment
//! variable that holds it and read by the component that needs it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use serde::Deserialize;

use henk_domain::allowlist::{AllowRule, Allowlist, Platform, RepoRef};
use henk_domain::discord::HenkIdentity;
use henk_domain::identity::{DiscordChannelId, DiscordRoleId, DiscordUserId, People, Person};
use henk_domain::mail::EmailAddress;
use henk_domain::marker::ModelId;
use henk_domain::review::{LaneName, LaneSpec};
use henk_llm::{Effort, MaxTokensParam, Provider, RetryPolicy};
use henk_mcp::McpServerConfig;

/// An example configuration, printed by `henk config example`.
pub const EXAMPLE: &str = include_str!("../../../henk.example.toml");

/// The configuration file as written.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The HTTP server and run links.
    #[serde(default)]
    pub server: ServerConfig,
    /// Discord ids.
    pub discord: DiscordConfig,
    /// Henk's mailbox.
    pub mail: MailConfig,
    /// Where Henk works.
    pub allowlist: AllowlistConfig,
    /// GitHub App identity.
    #[serde(default)]
    pub github: Option<GitHubConfig>,
    /// GitLab identity.
    #[serde(default)]
    pub gitlab: Option<GitLabConfig>,
    /// Models by id.
    #[serde(default)]
    pub models: BTreeMap<String, ModelFileConfig>,
    /// Review settings.
    #[serde(default)]
    pub review: ReviewConfig,
    /// Planning settings.
    #[serde(default)]
    pub planning: Option<PlanningConfig>,
    /// External MCP servers by alias.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// People Henk knows by id.
    #[serde(default)]
    pub people: Vec<PersonConfig>,
}

/// The HTTP server.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind address.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Public base URL run links point at, without a trailing slash.
    #[serde(default = "default_public_base_url")]
    pub public_base_url: String,
    /// SQLite file.
    #[serde(default = "default_database_path")]
    pub database_path: String,
    /// Env var with the GitHub webhook secret.
    #[serde(default = "default_github_webhook_secret_env")]
    pub github_webhook_secret_env: String,
    /// Env var with the GitLab webhook token.
    #[serde(default = "default_gitlab_webhook_token_env")]
    pub gitlab_webhook_token_env: String,
    /// Env var with the bearer token for `POST /plan` and `POST /review`.
    #[serde(default = "default_api_token_env")]
    pub api_token_env: String,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
fn default_public_base_url() -> String {
    "http://127.0.0.1:8080".to_owned()
}
fn default_database_path() -> String {
    "henk.db".to_owned()
}
fn default_github_webhook_secret_env() -> String {
    "HENK_GITHUB_WEBHOOK_SECRET".to_owned()
}
fn default_gitlab_webhook_token_env() -> String {
    "HENK_GITLAB_WEBHOOK_TOKEN".to_owned()
}
fn default_api_token_env() -> String {
    "HENK_API_TOKEN".to_owned()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            public_base_url: default_public_base_url(),
            database_path: default_database_path(),
            github_webhook_secret_env: default_github_webhook_secret_env(),
            gitlab_webhook_token_env: default_gitlab_webhook_token_env(),
            api_token_env: default_api_token_env(),
        }
    }
}

/// Discord ids.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    /// The one channel Henk works in.
    pub channel_id: DiscordChannelId,
    /// Henk's own user id.
    pub henk_user_id: DiscordUserId,
    /// Henk's role id, if he has one.
    pub henk_role_id: Option<DiscordRoleId>,
    /// The "Collega van Henk" role id.
    pub colleague_role_id: Option<DiscordRoleId>,
    /// Team Lead user ids.
    #[serde(default)]
    pub team_lead_ids: Vec<DiscordUserId>,
}

/// Henk's mailbox.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    /// The address Henk sends from and never replies to.
    pub address: EmailAddress,
}

/// Where Henk works.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AllowlistConfig {
    /// Single GitHub repositories, as `owner/name`.
    #[serde(default)]
    pub github_repositories: Vec<String>,
    /// GitHub users or organisations whose every repository is allowed.
    #[serde(default)]
    pub github_owners: Vec<String>,
    /// Single GitLab projects, as `group/subgroup/name`.
    #[serde(default)]
    pub gitlab_projects: Vec<String>,
    /// GitLab groups whose every project, including subgroups, is allowed.
    #[serde(default)]
    pub gitlab_groups: Vec<String>,
}

/// GitHub App identity. The private key is read from a file named by an
/// environment variable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubConfig {
    /// App id.
    pub app_id: u64,
    /// Installation id.
    pub installation_id: u64,
    /// Env var holding the path to the PEM private key.
    #[serde(default = "default_github_key_env")]
    pub private_key_path_env: String,
    /// The App's login as it appears on comments, such as `meneer-henk[bot]`.
    pub bot_login: String,
    /// API base URL.
    #[serde(default = "default_github_api_base")]
    pub api_base: String,
    /// Alias of the `[mcp.*]` server lanes read GitHub through.
    #[serde(default = "default_github_mcp")]
    pub mcp_server: String,
}

fn default_github_key_env() -> String {
    "GITHUB_APP_PRIVATE_KEY_PATH".to_owned()
}
fn default_github_api_base() -> String {
    "https://api.github.com".to_owned()
}
fn default_github_mcp() -> String {
    "github".to_owned()
}

/// GitLab identity.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitLabConfig {
    /// API base, such as `https://gitlab.com/api/v4`.
    pub api_url: String,
    /// Henk's GitLab username.
    pub username: String,
    /// Env var with the personal access token used for writes.
    #[serde(default = "default_gitlab_token_env")]
    pub token_env: String,
    /// Alias of the `[mcp.*]` server lanes read GitLab through.
    #[serde(default = "default_gitlab_mcp")]
    pub mcp_server: String,
    /// Alias of the `[mcp.*]` server Henk writes GitLab through.
    #[serde(default = "default_gitlab_write_mcp")]
    pub write_mcp_server: String,
}

fn default_gitlab_token_env() -> String {
    "GITLAB_PERSONAL_ACCESS_TOKEN".to_owned()
}
fn default_gitlab_mcp() -> String {
    "gitlab".to_owned()
}
fn default_gitlab_write_mcp() -> String {
    "gitlab-write".to_owned()
}

/// One model endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFileConfig {
    /// Wire format.
    pub provider: Provider,
    /// Base URL.
    pub base_url: String,
    /// Env var with the API key.
    pub api_key_env: String,
    /// Model name sent to the endpoint.
    pub model: String,
    /// Default output cap.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Per-attempt timeout in seconds.
    #[serde(default = "default_model_timeout_secs")]
    pub timeout_secs: u64,
    /// Which parameter carries the output cap (OpenAI style only).
    #[serde(default)]
    pub max_tokens_param: MaxTokensParam,
    /// Thinking effort (Anthropic style only): `low`, `medium`, `high`,
    /// `xhigh` or `max`. Left out, the model's own default applies.
    #[serde(default)]
    pub effort: Option<Effort>,
}

fn default_max_tokens() -> u32 {
    4096
}
fn default_model_timeout_secs() -> u64 {
    180
}

/// Review settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// The lanes.
    #[serde(default)]
    pub lanes: Vec<LaneFileConfig>,
    /// Wall-clock limit per lane, in seconds.
    #[serde(default = "default_lane_timeout_secs")]
    pub lane_timeout_secs: u64,
    /// Model calls per lane.
    #[serde(default = "default_lane_max_turns")]
    pub lane_max_turns: u32,
    /// Characters a lane's conversation may hold before old tool results
    /// are replaced by stubs. Every turn resends the whole conversation, so
    /// this bounds the cost of one turn.
    #[serde(default = "default_max_conversation_chars")]
    pub max_conversation_chars: usize,
    /// Turns, counted from the end, whose tool results are never stubbed.
    #[serde(default = "default_keep_recent_turns")]
    pub keep_recent_turns: u32,
    /// Reviews running at once.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Whether GitHub draft pull requests are reviewed (open question 1).
    #[serde(default)]
    pub github_drafts: bool,
    /// Fact-checking of findings before they are posted. Absent: findings
    /// are posted unchecked.
    #[serde(default)]
    pub fact_check: Option<FactCheckConfig>,
}

/// A second model checks every finding before it is posted (§3.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactCheckConfig {
    /// Model id from `[models]`.
    pub model: String,
    /// Model id from `[models]`, used when `model` wrote the finding itself
    /// or when its check could not be run.
    #[serde(default)]
    pub backup_model: Option<String>,
    /// Wall-clock limit per check, in seconds.
    #[serde(default = "default_fact_check_timeout_secs")]
    pub timeout_secs: u64,
    /// Model calls per check.
    #[serde(default = "default_fact_check_max_turns")]
    pub max_turns: u32,
}

fn default_fact_check_timeout_secs() -> u64 {
    180
}
fn default_fact_check_max_turns() -> u32 {
    12
}

fn default_lane_timeout_secs() -> u64 {
    600
}
fn default_max_conversation_chars() -> usize {
    160_000
}
fn default_keep_recent_turns() -> u32 {
    2
}
fn default_lane_max_turns() -> u32 {
    40
}
fn default_max_concurrent() -> usize {
    3
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            lanes: Vec::new(),
            lane_timeout_secs: default_lane_timeout_secs(),
            lane_max_turns: default_lane_max_turns(),
            max_conversation_chars: default_max_conversation_chars(),
            keep_recent_turns: default_keep_recent_turns(),
            max_concurrent: default_max_concurrent(),
            github_drafts: false,
            fact_check: None,
        }
    }
}

/// One lane.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneFileConfig {
    /// Name shown when the lane does not finish. Not a model name.
    pub name: String,
    /// Model id from `[models]`.
    pub model: String,
}

/// Planning settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningConfig {
    /// Model id from `[models]`.
    pub model: String,
    /// Time limit in seconds (open question 6).
    #[serde(default = "default_plan_timeout_secs")]
    pub timeout_secs: u64,
    /// Model calls.
    #[serde(default = "default_plan_max_turns")]
    pub max_turns: u32,
    /// Tracker changes per plan.
    #[serde(default = "default_change_budget")]
    pub change_budget: u32,
    /// Sub-issues per plan.
    #[serde(default = "default_sub_issue_cap")]
    pub sub_issue_cap: u32,
    /// The Team Lead on whose behalf CLI and API plans run.
    pub requester_id: DiscordUserId,
}

fn default_plan_timeout_secs() -> u64 {
    20 * 60
}
fn default_plan_max_turns() -> u32 {
    80
}
fn default_change_budget() -> u32 {
    20
}
fn default_sub_issue_cap() -> u32 {
    5
}

/// A known person.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonConfig {
    /// Discord user id.
    pub discord_id: DiscordUserId,
    /// How Henk addresses them.
    pub name: String,
    /// What they do on the team.
    pub role: String,
}

/// Why a configuration is unusable.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The TOML did not parse or had the wrong shape.
    #[error("invalid configuration: {0}")]
    Syntax(#[from] toml::de::Error),
    /// An allowlist entry is not a repository path.
    #[error("allowlist entry: {0}")]
    Allowlist(#[from] henk_domain::allowlist::RepoRefError),
    /// The allowlist is empty, so Henk would work nowhere.
    #[error("the allowlist is empty; Henk would work nowhere")]
    EmptyAllowlist,
    /// Henk is listed as his own Team Lead or colleague.
    #[error("Henk's own user id {0} appears in the roster")]
    HenkInRoster(DiscordUserId),
    /// Something refers to a model id that is not configured.
    #[error("{what} refers to unknown model {model:?}")]
    UnknownModel {
        /// Where the reference is.
        what: String,
        /// The id.
        model: String,
    },
    /// Lane names must be unique and must not name a model (open question 4).
    #[error("lane {0:?} is invalid: {1}")]
    Lane(String, String),
    /// A platform section refers to an MCP alias that is not configured.
    #[error("{what} refers to unknown MCP server {alias:?}")]
    UnknownMcpServer {
        /// Where the reference is.
        what: String,
        /// The alias.
        alias: String,
    },
    /// The planning requester is not a Team Lead.
    #[error("planning.requester_id {0} is not in discord.team_lead_ids")]
    PlanningRequester(DiscordUserId),
    /// A model configuration is unusable.
    #[error("model {0:?}: {1}")]
    Model(String, String),
}

/// The configuration, validated and turned into domain values.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The HTTP server.
    pub server: ServerConfig,
    /// Henk on Discord.
    pub henk: HenkIdentity,
    /// Henk's mail address.
    pub mail_address: EmailAddress,
    /// The roster.
    pub people: People,
    /// Where Henk works.
    pub allowlist: Allowlist,
    /// GitHub App.
    pub github: Option<GitHubConfig>,
    /// GitLab.
    pub gitlab: Option<GitLabConfig>,
    /// Models by id, without their secrets.
    pub models: BTreeMap<String, ModelFileConfig>,
    /// Review lanes.
    pub lanes: Vec<LaneSpec>,
    /// Lane limits.
    pub review: ReviewConfig,
    /// Planning.
    pub planning: Option<PlanningConfig>,
    /// MCP servers by alias.
    pub mcp: BTreeMap<String, McpServerConfig>,
}

impl Config {
    /// Parses the TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Syntax`] when the text is not valid or has unknown fields.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Validates the configuration and builds domain values from it.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] describing the first problem found.
    pub fn into_settings(self) -> Result<Settings, ConfigError> {
        let henk_id = self.discord.henk_user_id;
        if self.discord.team_lead_ids.contains(&henk_id)
            || self
                .people
                .iter()
                .any(|person| person.discord_id == henk_id)
        {
            return Err(ConfigError::HenkInRoster(henk_id));
        }

        let allowlist = build_allowlist(&self.allowlist)?;
        if allowlist.is_empty() {
            return Err(ConfigError::EmptyAllowlist);
        }

        validate_models(&self.models)?;
        let lanes = validate_lanes(&self.review.lanes, &self.models)?;
        if let Some(fact_check) = &self.review.fact_check {
            let models = std::iter::once(&fact_check.model).chain(&fact_check.backup_model);
            for model in models {
                if !self.models.contains_key(model) {
                    return Err(ConfigError::UnknownModel {
                        what: "review.fact_check".to_owned(),
                        model: model.clone(),
                    });
                }
            }
        }
        if let Some(planning) = &self.planning {
            if !self.models.contains_key(&planning.model) {
                return Err(ConfigError::UnknownModel {
                    what: "planning".to_owned(),
                    model: planning.model.clone(),
                });
            }
            if !self.discord.team_lead_ids.contains(&planning.requester_id) {
                return Err(ConfigError::PlanningRequester(planning.requester_id));
            }
        }
        validate_mcp_references(self.github.as_ref(), self.gitlab.as_ref(), &self.mcp)?;

        let people = People::new(
            self.discord.team_lead_ids.iter().copied(),
            self.discord.colleague_role_id,
            self.people.into_iter().map(|person| {
                (
                    person.discord_id,
                    Person {
                        name: person.name,
                        role: person.role,
                    },
                )
            }),
        );

        Ok(Settings {
            server: self.server,
            henk: HenkIdentity {
                user: henk_id,
                role: self.discord.henk_role_id,
                channel: self.discord.channel_id,
            },
            mail_address: self.mail.address,
            people,
            allowlist,
            github: self.github,
            gitlab: self.gitlab,
            models: self.models,
            lanes,
            review: self.review,
            planning: self.planning,
            mcp: self.mcp,
        })
    }
}

fn validate_models(models: &BTreeMap<String, ModelFileConfig>) -> Result<(), ConfigError> {
    for (id, model) in models {
        if model.model.trim().is_empty() {
            return Err(ConfigError::Model(
                id.clone(),
                "model name is empty".to_owned(),
            ));
        }
        if !(model.base_url.starts_with("https://") || model.base_url.starts_with("http://")) {
            return Err(ConfigError::Model(
                id.clone(),
                "base_url must start with http(s)://".to_owned(),
            ));
        }
        if model.effort.is_some() && model.provider != Provider::Anthropic {
            return Err(ConfigError::Model(
                id.clone(),
                "effort applies to provider \"anthropic\" only".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_lanes(
    lanes: &[LaneFileConfig],
    models: &BTreeMap<String, ModelFileConfig>,
) -> Result<Vec<LaneSpec>, ConfigError> {
    let mut names = BTreeSet::new();
    let mut specs = Vec::new();
    for lane in lanes {
        if !models.contains_key(&lane.model) {
            return Err(ConfigError::UnknownModel {
                what: format!("lane {}", lane.name),
                model: lane.model.clone(),
            });
        }
        if lane.name.trim().is_empty() {
            return Err(ConfigError::Lane(
                lane.name.clone(),
                "name is empty".to_owned(),
            ));
        }
        if !names.insert(lane.name.clone()) {
            return Err(ConfigError::Lane(
                lane.name.clone(),
                "duplicate name".to_owned(),
            ));
        }
        let names_a_model = models
            .values()
            .any(|m| m.model.eq_ignore_ascii_case(&lane.name))
            || models.contains_key(&lane.name);
        if names_a_model {
            return Err(ConfigError::Lane(
                lane.name.clone(),
                "lane names must not name a model".to_owned(),
            ));
        }
        let model = ModelId::parse(lane.model.clone())
            .map_err(|e| ConfigError::Lane(lane.name.clone(), e.to_string()))?;
        specs.push(LaneSpec {
            name: LaneName::new(lane.name.clone()),
            model,
        });
    }
    Ok(specs)
}

fn validate_mcp_references(
    github: Option<&GitHubConfig>,
    gitlab: Option<&GitLabConfig>,
    mcp: &BTreeMap<String, McpServerConfig>,
) -> Result<(), ConfigError> {
    let mut references: Vec<(&str, &String)> = Vec::new();
    if let Some(github) = github {
        references.push(("github.mcp_server", &github.mcp_server));
    }
    if let Some(gitlab) = gitlab {
        references.push(("gitlab.mcp_server", &gitlab.mcp_server));
        references.push(("gitlab.write_mcp_server", &gitlab.write_mcp_server));
    }
    for (what, alias) in references {
        if !mcp.contains_key(alias) {
            return Err(ConfigError::UnknownMcpServer {
                what: what.to_owned(),
                alias: alias.clone(),
            });
        }
    }
    Ok(())
}

fn build_allowlist(config: &AllowlistConfig) -> Result<Allowlist, ConfigError> {
    let mut rules = Vec::new();
    for path in &config.github_repositories {
        rules.push(AllowRule::Repository(RepoRef::parse(
            Platform::GitHub,
            path,
        )?));
    }
    for owner in &config.github_owners {
        rules.push(AllowRule::Owner {
            platform: Platform::GitHub,
            owner: owner.trim().to_owned(),
        });
    }
    for path in &config.gitlab_projects {
        rules.push(AllowRule::Repository(RepoRef::parse(
            Platform::GitLab,
            path,
        )?));
    }
    for group in &config.gitlab_groups {
        rules.push(AllowRule::Owner {
            platform: Platform::GitLab,
            owner: group.trim().trim_matches('/').to_owned(),
        });
    }
    Ok(Allowlist::new(rules))
}

impl ModelFileConfig {
    /// Turns the file entry into a client configuration, reading the key
    /// from the environment.
    ///
    /// # Errors
    ///
    /// Returns the name of the missing environment variable.
    pub fn to_client_config(
        &self,
        lookup_env: impl Fn(&str) -> Option<String>,
    ) -> Result<henk_llm::ModelConfig, String> {
        let key = lookup_env(&self.api_key_env).ok_or_else(|| self.api_key_env.clone())?;
        Ok(henk_llm::ModelConfig {
            provider: self.provider,
            base_url: self.base_url.clone(),
            api_key: key.into(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            timeout: Duration::from_secs(self.timeout_secs),
            retry: RetryPolicy::default(),
            max_tokens_param: self.max_tokens_param,
            effort: self.effort,
        })
    }
}

impl Settings {
    /// Public link of a run.
    #[must_use]
    pub fn run_link(&self, run: &henk_domain::run::RunId) -> String {
        format!(
            "{}/runs/{run}",
            self.server.public_base_url.trim_end_matches('/')
        )
    }

    /// A plain-text description of what Henk would work with.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Server:          {} (public {})",
            self.server.bind, self.server.public_base_url
        );
        let _ = writeln!(
            out,
            "Secrets from:    ${} ${} ${}",
            self.server.github_webhook_secret_env,
            self.server.gitlab_webhook_token_env,
            self.server.api_token_env
        );
        let _ = writeln!(out, "Discord channel: {}", self.henk.channel);
        let _ = writeln!(out, "Henk's user id:  {}", self.henk.user);
        let _ = writeln!(out, "Mail address:    {}", self.mail_address);
        let leads: Vec<String> = self.people.team_leads().map(|id| id.to_string()).collect();
        let _ = writeln!(
            out,
            "Team leads:      {}",
            if leads.is_empty() {
                "none".to_owned()
            } else {
                leads.join(", ")
            }
        );
        let _ = writeln!(out, "Allowlist:");
        for rule in self.allowlist.rules() {
            match rule {
                AllowRule::Repository(repo) => {
                    let _ = writeln!(out, "  {} repository {}", repo.platform(), repo.path());
                }
                AllowRule::Owner { platform, owner } => {
                    let _ = writeln!(out, "  {platform} everything under {owner}");
                }
            }
        }
        let _ = writeln!(out, "Models:");
        for (id, model) in &self.models {
            let _ = writeln!(
                out,
                "  {id}: {:?} {} at {} (key from ${})",
                model.provider, model.model, model.base_url, model.api_key_env
            );
        }
        let _ = writeln!(
            out,
            "Review limits:   {} at once, {}s and {} turns per lane",
            self.review.max_concurrent, self.review.lane_timeout_secs, self.review.lane_max_turns
        );
        let _ = writeln!(out, "Review lanes:");
        for lane in &self.lanes {
            let _ = writeln!(out, "  {} on model {}", lane.name, lane.model);
        }
        if let Some(planning) = &self.planning {
            let _ = writeln!(
                out,
                "Planning:        model {} for Team Lead {}; {}s, {} turns, {} changes, {} sub-issues",
                planning.model,
                planning.requester_id,
                planning.timeout_secs,
                planning.max_turns,
                planning.change_budget,
                planning.sub_issue_cap
            );
        }
        let _ = writeln!(out, "MCP servers:");
        for (alias, server) in &self.mcp {
            let how = server.command.as_deref().map_or_else(
                || format!("http {}", server.url.as_deref().unwrap_or("?")),
                |c| format!("stdio {c} {}", server.args.join(" ")),
            );
            let _ = writeln!(out, "  {alias}: {how}");
        }
        if let Some(github) = &self.github {
            let _ = writeln!(
                out,
                "GitHub App:      id {} installation {} as {}",
                github.app_id, github.installation_id, github.bot_login
            );
        }
        if let Some(gitlab) = &self.gitlab {
            let _ = writeln!(
                out,
                "GitLab:          {} as {} (token from ${})",
                gitlab.api_url, gitlab.username, gitlab.token_env
            );
        }
        out.trim_end().to_owned()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const MINIMAL: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["docspec"]
"#;

    #[test]
    fn example_config_is_valid() {
        let settings = Config::parse(EXAMPLE)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(settings.lanes.len(), 2);
        assert!(settings.github.is_some());
        assert!(settings.gitlab.is_some());
        assert_eq!(settings.mcp.len(), 3);
        assert!(settings.models.contains_key("proxy-fast"));
        let scratch = RepoRef::parse(Platform::GitHub, "StephanMeijer/scratch-repo").unwrap();
        assert!(settings.allowlist.allows(&scratch));
        assert_eq!(
            settings.run_link(&henk_domain::run::RunId::parse("r-1").unwrap()),
            "https://henk.example.com/runs/r-1"
        );
    }

    #[test]
    fn minimal_config_is_valid() {
        let settings = Config::parse(MINIMAL)
            .and_then(Config::into_settings)
            .unwrap();
        assert!(settings.lanes.is_empty());
        assert_eq!(settings.server.bind, "127.0.0.1:8080");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let text = format!("{EXAMPLE}\n[surprise]\nkey = 1\n");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Syntax(_))));
    }

    #[test]
    fn empty_allowlist_is_rejected() {
        let text = MINIMAL.replace("github_owners = [\"docspec\"]", "");
        assert!(matches!(
            Config::parse(&text).and_then(Config::into_settings),
            Err(ConfigError::EmptyAllowlist)
        ));
    }

    #[test]
    fn henk_cannot_be_his_own_team_lead() {
        let text = MINIMAL.replace("team_lead_ids = [3]", "team_lead_ids = [2]");
        let result = Config::parse(&text).and_then(Config::into_settings);
        assert!(matches!(result, Err(ConfigError::HenkInRoster(id)) if id.get() == 2));
    }

    #[test]
    fn lanes_must_reference_models_and_not_name_them() {
        let text = format!(
            "{MINIMAL}\n[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x\"\napi_key_env = \"K\"\nmodel = \"gpt\"\n[review]\nlanes = [{{ name = \"a\", model = \"nope\" }}]\n"
        );
        assert!(matches!(
            Config::parse(&text).and_then(Config::into_settings),
            Err(ConfigError::UnknownModel { .. })
        ));
        let text = text
            .replace("model = \"nope\"", "model = \"m\"")
            .replace("name = \"a\"", "name = \"gpt\"");
        assert!(matches!(
            Config::parse(&text).and_then(Config::into_settings),
            Err(ConfigError::Lane(..))
        ));
    }

    #[test]
    fn planning_requester_must_be_a_team_lead() {
        let text = format!(
            "{MINIMAL}\n[models.m]\nprovider = \"anthropic\"\nbase_url = \"https://x\"\napi_key_env = \"K\"\nmodel = \"c\"\n[planning]\nmodel = \"m\"\nrequester_id = 99\n"
        );
        assert!(matches!(
            Config::parse(&text).and_then(Config::into_settings),
            Err(ConfigError::PlanningRequester(_))
        ));
    }

    #[test]
    fn effort_is_for_anthropic_models_only() {
        let model = "[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x\"\napi_key_env = \"K\"\nmodel = \"gpt\"\neffort = \"high\"\n";
        let text = format!("{MINIMAL}\n{model}");
        assert!(matches!(
            Config::parse(&text).and_then(Config::into_settings),
            Err(ConfigError::Model(..))
        ));
        let text = text.replace("open_ai", "anthropic");
        let settings = Config::parse(&text)
            .and_then(Config::into_settings)
            .unwrap();
        assert_eq!(settings.models.get("m").unwrap().effort, Some(Effort::High));
        let bad = text.replace("\"high\"", "\"extreme\"");
        assert!(matches!(Config::parse(&bad), Err(ConfigError::Syntax(_))));
    }

    #[test]
    fn fact_check_models_must_exist() {
        let model = "[models.m]\nprovider = \"anthropic\"\nbase_url = \"https://x\"\napi_key_env = \"K\"\nmodel = \"c\"\n";
        let text = format!(
            "{MINIMAL}\n{model}[review.fact_check]\nmodel = \"m\"\nbackup_model = \"nope\"\n"
        );
        assert!(matches!(
            Config::parse(&text).and_then(Config::into_settings),
            Err(ConfigError::UnknownModel { ref what, .. }) if what == "review.fact_check"
        ));
        let text = text.replace("backup_model = \"nope\"\n", "");
        let settings = Config::parse(&text)
            .and_then(Config::into_settings)
            .unwrap();
        let fact_check = settings.review.fact_check.unwrap();
        assert_eq!((fact_check.timeout_secs, fact_check.max_turns), (180, 12));
    }

    #[test]
    fn github_section_needs_its_mcp_server() {
        let text = format!(
            "{MINIMAL}\n[github]\napp_id = 1\ninstallation_id = 2\nbot_login = \"h[bot]\"\n"
        );
        assert!(matches!(
            Config::parse(&text).and_then(Config::into_settings),
            Err(ConfigError::UnknownMcpServer { .. })
        ));
    }
}
