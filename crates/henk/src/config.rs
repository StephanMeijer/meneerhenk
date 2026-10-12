//! Configuration: identities, the allowlist, models, lanes and MCP servers.
//!
//! No credential lives here (§8.4). Every secret is named by the environment
//! variable that holds it and read by the component that needs it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use serde::Deserialize;

use henk_domain::allowlist::{AllowRule, Allowlist, Platform, RepoRef};
use henk_domain::commit::{CommitPerson, Email, IdentityError, TrailerPolicy};
use henk_domain::discord::HenkIdentity;
use henk_domain::identity::{DiscordChannelId, DiscordRoleId, DiscordUserId, People, Person};
use henk_domain::mail::EmailAddress;
use henk_domain::role::Role;
use henk_domain::skill::SkillName;
use henk_domain::workspace::{BackendKind, Limits, Profile, Toolchain, WorkspacePolicy};
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
    /// Where run records live.
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
    /// The web dashboard; absent means none.
    #[serde(default)]
    pub dashboard: Option<DashboardConfig>,
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
    /// The agents Henk runs (#322): planner, reviewer and executor.
    #[serde(default)]
    pub roles: RolesConfig,
    /// Planning settings.
    #[serde(default)]
    pub planning: Option<PlanningConfig>,
    /// The address run's section before roles (#322), read only to refuse
    /// it with a message that says what replaced it.
    #[serde(default)]
    address: Option<toml::Value>,
    /// Where a model's tools and the project's checks run.
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    /// External MCP servers by alias.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Henk's own MCP server at `/mcp` (#248, #249); absent means off.
    #[serde(default)]
    pub mcp_server: Option<McpServeConfig>,
    /// Where the team's skills live. Absent: no agent has skills.
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
    /// People Henk knows by id.
    #[serde(default)]
    pub people: Vec<PersonConfig>,
    /// Limits every model session shares.
    #[serde(default)]
    pub agent: AgentFileConfig,
}

/// Henk's own MCP server (#248, #249): which clients may use `/mcp`, and
/// what each may do. Only token names and variable names live here; the
/// tokens are in the environment (§8.4).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServeConfig {
    /// Whether `/mcp` answers at all.
    #[serde(default)]
    pub enabled: bool,
    /// The clients' tokens.
    #[serde(default)]
    pub tokens: Vec<McpTokenConfig>,
}

/// One MCP client's token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpTokenConfig {
    /// What the client is called on the record: a start through it is
    /// asked by `mcp:<name>`.
    pub name: String,
    /// The environment variable that holds the token.
    pub env: String,
    /// What it may do.
    pub scope: McpScope,
}

/// What an MCP client may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpScope {
    /// List and read runs, events and health.
    Read,
    /// Also start and cancel runs.
    Write,
}

impl McpScope {
    /// The scope's word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// Checks the ways in besides the hooks: the dashboard and the MCP server.
fn validate_ways_in(
    dashboard: Option<&DashboardConfig>,
    mcp_server: Option<&McpServeConfig>,
) -> Result<(), ConfigError> {
    if let Some(dashboard) = dashboard {
        validate_dashboard(dashboard)?;
    }
    if let Some(mcp_server) = mcp_server {
        validate_mcp_serve(mcp_server)?;
    }
    Ok(())
}

/// Checks the MCP server's tokens: names a requester can carry, each once,
/// each with its own variable; and at least one when it is on. Two tokens
/// on one variable would be one secret with two scopes.
fn validate_mcp_serve(config: &McpServeConfig) -> Result<(), ConfigError> {
    let fail = |text: String| Err(ConfigError::McpServe(text));
    if config.enabled && config.tokens.is_empty() {
        return fail("mcp_server.enabled needs at least one [[mcp_server.tokens]]".to_owned());
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut envs = std::collections::BTreeSet::new();
    for token in &config.tokens {
        let fine = !token.name.is_empty()
            && token
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !fine {
            return fail(format!(
                "mcp_server token name {:?} must be lowercase letters, digits and dashes",
                token.name
            ));
        }
        if !seen.insert(token.name.as_str()) {
            return fail(format!("mcp_server token {:?} is named twice", token.name));
        }
        if token.env.trim().is_empty() {
            return fail(format!("mcp_server token {:?} needs env", token.name));
        }
        if !envs.insert(token.env.trim()) {
            return fail(format!(
                "mcp_server token {:?} uses env {:?}, which another token uses too",
                token.name, token.env
            ));
        }
    }
    Ok(())
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
    /// Deprecated: the `SQLite` file. Use `[database]`.
    #[serde(default)]
    pub database_path: Option<String>,
    /// Env var with the GitHub webhook secret.
    #[serde(default = "default_github_webhook_secret_env")]
    pub github_webhook_secret_env: String,
    /// Env var with the GitLab webhook token.
    #[serde(default = "default_gitlab_webhook_token_env")]
    pub gitlab_webhook_token_env: String,
    /// Env var with the bearer token for `POST /plan` and `POST /review`.
    #[serde(default = "default_api_token_env")]
    pub api_token_env: String,
    /// Inbound events, their outcomes and session transcripts older than
    /// this many days are deleted while serving. Runs are kept: their links
    /// are posted.
    #[serde(default = "default_keep_events_days")]
    pub keep_events_days: u32,
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
const fn default_keep_events_days() -> u32 {
    30
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            public_base_url: default_public_base_url(),
            database_path: None,
            github_webhook_secret_env: default_github_webhook_secret_env(),
            gitlab_webhook_token_env: default_gitlab_webhook_token_env(),
            api_token_env: default_api_token_env(),
            keep_events_days: default_keep_events_days(),
        }
    }
}

/// The web dashboard (#36): who may sign in with GitHub, and where the
/// secrets are. Only ids and variable names live here (§2, §8.4).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    /// GitHub user ids that may sign in.
    pub allowed_github_ids: Vec<u64>,
    /// Env var with the GitHub OAuth App's client id.
    #[serde(default = "default_dashboard_client_id_env")]
    pub client_id_env: String,
    /// Env var with the GitHub OAuth App's client secret.
    #[serde(default = "default_dashboard_client_secret_env")]
    pub client_secret_env: String,
    /// Env var with the key that signs sessions, at least 32 bytes.
    #[serde(default = "default_dashboard_session_key_env")]
    pub session_key_env: String,
    /// How long a sign-in lasts.
    #[serde(default = "default_session_hours")]
    pub session_hours: u32,
    /// GitHub's web address, for the sign-in page.
    #[serde(default = "default_github_web_base")]
    pub github_web_base: String,
    /// GitHub's API, for who signed in.
    #[serde(default = "default_github_api_base")]
    pub github_api_base: String,
}

fn default_dashboard_client_id_env() -> String {
    "HENK_DASHBOARD_CLIENT_ID".to_owned()
}
fn default_dashboard_client_secret_env() -> String {
    "HENK_DASHBOARD_CLIENT_SECRET".to_owned()
}
fn default_dashboard_session_key_env() -> String {
    "HENK_DASHBOARD_SESSION_KEY".to_owned()
}
fn default_session_hours() -> u32 {
    12
}
fn default_github_web_base() -> String {
    "https://github.com".to_owned()
}
fn default_github_api_base() -> String {
    "https://api.github.com".to_owned()
}

/// Where run records live.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum DatabaseConfig {
    /// One local `SQLite` file.
    Sqlite {
        /// The file; created when missing.
        #[serde(default = "default_database_path")]
        path: String,
    },
    /// A `PostgreSQL` server. The URL holds the password, so the file names
    /// only the environment variable that has it (§8.4).
    Postgres {
        /// Env var with the connection URL.
        url_env: String,
    },
}

impl DatabaseConfig {
    /// Where records go, in words, without any secret.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Sqlite { path } => format!("SQLite {path}"),
            Self::Postgres { url_env } => format!("PostgreSQL (URL from ${url_env})"),
        }
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self::Sqlite {
            path: default_database_path(),
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
    /// Env var with the personal access token: the MCP servers sign in with
    /// it, and Henk pushes (§3.5) and reads the merge request with it.
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
    /// Prompt caching (Anthropic style only): the system prompt and the end
    /// of the conversation are marked, so each turn reads what the last one
    /// wrote. Set false for a proxy that rejects `cache_control`.
    #[serde(default = "default_prompt_cache")]
    pub prompt_cache: bool,
}

fn default_prompt_cache() -> bool {
    true
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
    /// Characters a session's conversation may hold before old tool results
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
    /// The reviewer↔executor loop (#284): a review is one reviewer and one
    /// executor taking turns. Absent: reviews are off.
    #[serde(default, rename = "loop")]
    pub r#loop: Option<LoopConfig>,
    /// Changed files no review reads (§3.2): lockfiles and generated
    /// changelogs by default. A pattern without `/` matches the file name at
    /// any depth; `**` matches any number of directories. Setting the key
    /// replaces the default; `ignore = []` reviews everything.
    #[serde(default = "default_review_ignore")]
    pub ignore: Vec<String>,
    /// The keys of the lanes and the fact-check (#309), read only to refuse
    /// them with a message that says what replaced them.
    #[serde(default)]
    lanes: Option<toml::Value>,
    /// See `lanes`.
    #[serde(default)]
    lane_timeout_secs: Option<toml::Value>,
    /// See `lanes`.
    #[serde(default)]
    lane_max_turns: Option<toml::Value>,
    /// See `lanes`.
    #[serde(default)]
    fact_check: Option<toml::Value>,
}

impl ReviewConfig {
    /// The first key of the lanes or the fact-check (#309) the file still
    /// has, as it is written there.
    fn removed_key(&self) -> Option<&'static str> {
        [
            (self.lanes.is_some(), "[review].lanes"),
            (
                self.lane_timeout_secs.is_some(),
                "[review].lane_timeout_secs",
            ),
            (self.lane_max_turns.is_some(), "[review].lane_max_turns"),
            (self.fact_check.is_some(), "[review.fact_check]"),
        ]
        .into_iter()
        .find_map(|(present, key)| present.then_some(key))
    }
}

fn default_review_ignore() -> Vec<String> {
    [
        "**/Cargo.lock",
        "**/package-lock.json",
        "**/yarn.lock",
        "**/pnpm-lock.yaml",
        "**/go.sum",
        "**/poetry.lock",
        "**/uv.lock",
        "CHANGELOG.md",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// The team's skills: one folder per skill, each with a `SKILL.md`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// The folder holding the skill folders, relative to the configuration
    /// file's own folder unless absolute.
    pub dir: String,
}

/// The reviewer↔executor loop (#284): a reviewer reviews, an executor fixes what
/// holds and pushes to the pull request's branch, and each resumes its own
/// conversation the next round.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopConfig {
    /// Rounds at most: one round is a reviewer turn and an executor turn.
    #[serde(default = "default_loop_max_rounds")]
    pub max_rounds: u32,
    /// Wall-clock limit for the whole loop, in seconds (#286): no round
    /// starts after it, and a round under way gets only what is left.
    #[serde(default = "default_loop_run_timeout_secs")]
    pub run_timeout_secs: u64,
    /// The keys that moved to the roles (#322), read only to refuse them
    /// with a message that says where they went.
    #[serde(default)]
    reviewer: Option<toml::Value>,
    /// See `reviewer`.
    #[serde(default)]
    fixer: Option<toml::Value>,
    /// See `reviewer`.
    #[serde(default)]
    round_timeout_secs: Option<toml::Value>,
    /// See `reviewer`.
    #[serde(default)]
    round_max_turns: Option<toml::Value>,
}

impl LoopConfig {
    /// The first key that moved to the roles (#322), and where it went.
    fn moved_key(&self) -> Option<(&'static str, &'static str)> {
        [
            (
                self.reviewer.is_some(),
                ("[review.loop].reviewer", "[roles.reviewer].model"),
            ),
            (
                self.fixer.is_some(),
                ("[review.loop].fixer", "[roles.executor].model"),
            ),
            (
                self.round_timeout_secs.is_some(),
                (
                    "[review.loop].round_timeout_secs",
                    "timeout_secs on [roles.reviewer] and [roles.executor]",
                ),
            ),
            (
                self.round_max_turns.is_some(),
                (
                    "[review.loop].round_max_turns",
                    "max_turns on [roles.reviewer] and [roles.executor]",
                ),
            ),
        ]
        .into_iter()
        .find_map(|(present, keys)| present.then_some(keys))
    }
}

fn default_loop_max_rounds() -> u32 {
    50
}

fn default_loop_run_timeout_secs() -> u64 {
    3600
}

fn default_max_conversation_chars() -> usize {
    160_000
}
fn default_keep_recent_turns() -> u32 {
    2
}
fn default_max_concurrent() -> usize {
    3
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            max_conversation_chars: default_max_conversation_chars(),
            keep_recent_turns: default_keep_recent_turns(),
            max_concurrent: default_max_concurrent(),
            github_drafts: false,
            r#loop: None,
            ignore: default_review_ignore(),
            lanes: None,
            lane_timeout_secs: None,
            lane_max_turns: None,
            fact_check: None,
        }
    }
}

/// Limits every model session shares: the review loop's sessions and the
/// planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentFileConfig {
    /// Identical tool calls in a row a session may make. The next one is
    /// refused, and one more in a later turn ends the session as stuck. 0
    /// turns the guard off.
    pub max_repeated_calls: u32,
    /// Bytes of each tool call's arguments the run record keeps (#190);
    /// longer ones are cut, with their length kept. 0 keeps none.
    pub record_argument_bytes: usize,
}

impl Default for AgentFileConfig {
    fn default() -> Self {
        Self {
            max_repeated_calls: henk_domain::repeat::DEFAULT_LIMIT,
            record_argument_bytes: 4096,
        }
    }
}

/// Planning settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningConfig {
    /// Tracker changes per plan.
    #[serde(default = "default_change_budget")]
    pub change_budget: u32,
    /// Sub-issues per plan.
    #[serde(default = "default_sub_issue_cap")]
    pub sub_issue_cap: u32,
    /// The reviewer's rounds on a drafted plan, at most (#328).
    #[serde(default = "default_plan_max_rounds")]
    pub max_rounds: u32,
    /// The time the planner and the reviewer have together, in seconds.
    #[serde(default = "default_plan_run_timeout_secs")]
    pub run_timeout_secs: u64,
    /// The Team Lead on whose behalf CLI and API plans run.
    pub requester_id: DiscordUserId,
    /// The keys that moved to `[roles.planner]` (#322), read only to refuse
    /// them with a message that says where they went.
    #[serde(default)]
    model: Option<toml::Value>,
    /// See `model`.
    #[serde(default)]
    timeout_secs: Option<toml::Value>,
    /// See `model`.
    #[serde(default)]
    max_turns: Option<toml::Value>,
    /// See `model`.
    #[serde(default)]
    skills: Option<toml::Value>,
}

impl PlanningConfig {
    /// The first key that moved to `[roles.planner]` (#322), and where.
    fn moved_key(&self) -> Option<(&'static str, &'static str)> {
        [
            (
                self.model.is_some(),
                ("[planning].model", "[roles.planner].model"),
            ),
            (
                self.timeout_secs.is_some(),
                ("[planning].timeout_secs", "[roles.planner].timeout_secs"),
            ),
            (
                self.max_turns.is_some(),
                ("[planning].max_turns", "[roles.planner].max_turns"),
            ),
            (
                self.skills.is_some(),
                ("[planning].skills", "[roles.planner].skills"),
            ),
        ]
        .into_iter()
        .find_map(|(present, keys)| present.then_some(keys))
    }
}

/// The agents Henk runs (#322), one section each. The review loop needs
/// the reviewer and the executor; planning needs the planner.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolesConfig {
    /// `[roles.planner]`.
    #[serde(default)]
    pub planner: Option<RoleConfig>,
    /// `[roles.reviewer]`.
    #[serde(default)]
    pub reviewer: Option<RoleConfig>,
    /// `[roles.executor]`: the agent that changes the code and pushes.
    #[serde(default)]
    pub executor: Option<ExecutorConfig>,
}

/// One role's agent: its model, tools, skills and limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleConfig {
    /// Model id from `[models]`.
    pub model: String,
    /// The tools it gets, within its ceiling (#171). Absent: the role's
    /// defaults.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// The skills it may load.
    #[serde(default)]
    pub skills: Vec<SkillName>,
    /// Model calls per turn: a planner's run, or a reviewer's round. Absent:
    /// 80 for the planner, 40 for the reviewer.
    #[serde(default)]
    max_turns: Option<u32>,
    /// Wall-clock limit per turn, in seconds. Absent: 1200 for the planner,
    /// 600 for the reviewer.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl RoleConfig {
    /// Model calls per turn for `role`.
    #[must_use]
    pub fn max_turns(&self, role: Role) -> u32 {
        self.max_turns.unwrap_or(match role {
            Role::Planner => 80,
            Role::Reviewer | Role::Executor => default_role_max_turns(),
        })
    }

    /// Wall-clock limit per turn for `role`, in seconds.
    #[must_use]
    pub fn timeout_secs(&self, role: Role) -> u64 {
        self.timeout_secs.unwrap_or(match role {
            Role::Planner => 1200,
            Role::Reviewer | Role::Executor => default_role_timeout_secs(),
        })
    }

    /// The tools `role` gets: the configured list, checked when the
    /// configuration was loaded, or the role's defaults.
    #[must_use]
    pub fn tools(&self, role: Role) -> Vec<String> {
        granted(role, self.tools.as_deref())
    }
}

/// The tools a role gets from its configured list; the defaults for a
/// list the load would have refused.
fn granted(role: Role, configured: Option<&[String]>) -> Vec<String> {
    role.narrow(configured)
        .unwrap_or_else(|_| role.default_tools())
}

fn default_role_max_turns() -> u32 {
    40
}
fn default_role_timeout_secs() -> u64 {
    600
}

/// The executor (#322): the agent that changes the code, and how what it
/// changes becomes a commit and a push (§3.5).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    /// Model id from `[models]`.
    pub model: String,
    /// The tools it gets, within its ceiling (#171). Absent: the
    /// executor's defaults.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// The skills it may load.
    #[serde(default)]
    pub skills: Vec<SkillName>,
    /// Model calls per turn.
    #[serde(default = "default_role_max_turns")]
    pub max_turns: u32,
    /// Wall-clock limit per turn, in seconds.
    #[serde(default = "default_role_timeout_secs")]
    pub timeout_secs: u64,
    /// Files one push may change.
    #[serde(default = "default_max_changed_files")]
    pub max_changed_files: usize,
    /// The project's checks, each an argument list such as
    /// `["cargo", "test"]`, run in the workspace with an empty environment
    /// and `[workspace] command_secs` each.
    #[serde(default)]
    pub check_commands: Vec<Vec<String>>,
    /// The Team Lead on whose behalf CLI and API runs push.
    pub requester_id: DiscordUserId,
    /// Who Henk commits as, author and committer alike. Absent: the App's
    /// own `meneer-henk[bot]` noreply address.
    #[serde(default)]
    pub identity: Option<CommitIdentityConfig>,
    /// The trailers of every commit, unless a repository says otherwise.
    #[serde(default)]
    pub trailers: TrailerConfig,
    /// Trailer settings per repository, by `owner/name`.
    #[serde(default)]
    pub repositories: BTreeMap<String, TrailerConfig>,
}

/// A name and email to commit as.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitIdentityConfig {
    /// The name, such as `Meneer Henk`.
    pub name: String,
    /// The email address.
    pub email: String,
}

impl CommitIdentityConfig {
    fn person(&self) -> Result<CommitPerson, IdentityError> {
        CommitPerson::new(&self.name, Email::parse(&self.email)?)
    }
}

/// Which trailers a commit of Henk's carries. A switch left out keeps the
/// default, or for a repository the `[roles.executor.trailers]` value.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailerConfig {
    /// Henk's `Signed-off-by`. Default on.
    pub henk_signoff: Option<bool>,
    /// The requester's `Co-authored-by`. Default on.
    pub requester_coauthor: Option<bool>,
    /// The requester's `Signed-off-by`. Default off: turning it on states
    /// that requesters certify the DCO for changes they asked for.
    pub requester_signoff: Option<bool>,
}

impl TrailerConfig {
    fn over(self, base: TrailerPolicy) -> TrailerPolicy {
        TrailerPolicy {
            henk_signoff: self.henk_signoff.unwrap_or(base.henk_signoff),
            requester_coauthor: self.requester_coauthor.unwrap_or(base.requester_coauthor),
            requester_signoff: self.requester_signoff.unwrap_or(base.requester_signoff),
        }
    }
}

impl ExecutorConfig {
    /// The tools the executor gets: the configured list, checked when the
    /// configuration was loaded, or its defaults.
    #[must_use]
    pub fn tools(&self) -> Vec<String> {
        granted(Role::Executor, self.tools.as_deref())
    }

    /// The trailers for a commit to `repo`: its override, over
    /// `[roles.executor.trailers]`, over the defaults.
    #[must_use]
    pub fn trailer_policy(&self, repo: &RepoRef) -> TrailerPolicy {
        let base = self.trailers.over(TrailerPolicy::default());
        self.repositories
            .iter()
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(&repo.path()))
            .map_or(base, |(_, config)| config.over(base))
    }

    /// Who Henk commits as when configured; `None` for the App's account.
    ///
    /// # Errors
    ///
    /// Returns [`IdentityError`] for a bad name or email; `config check`
    /// refuses those first.
    pub fn henk_identity(&self) -> Result<Option<CommitPerson>, IdentityError> {
        self.identity
            .as_ref()
            .map(CommitIdentityConfig::person)
            .transpose()
    }

    /// Every trailer setting in force: the default, then each repository.
    fn policies(&self) -> impl Iterator<Item = TrailerPolicy> + '_ {
        let base = self.trailers.over(TrailerPolicy::default());
        std::iter::once(base).chain(self.repositories.values().map(move |c| c.over(base)))
    }
}

fn default_max_changed_files() -> usize {
    20
}

/// Where a model's tools and the project's checks run: the default
/// profile's backend and limits, named profiles, and which repository uses
/// which profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Where workspaces run.
    pub backend: BackendKind,
    /// The image a backend starts from; not on the host.
    pub image: Option<String>,
    /// Seconds one command may run.
    pub command_secs: u64,
    /// Seconds all commands of one run may take together.
    pub run_secs: u64,
    /// Bytes of output kept per command.
    pub output_bytes: u64,
    /// Memory in MiB.
    pub memory_mib: Option<u64>,
    /// CPUs.
    pub cpus: Option<u64>,
    /// Processes.
    pub pids: Option<u64>,
    /// Disk in MiB.
    pub disk_mib: Option<u64>,
    /// Setup commands run before the model starts (#93), in order.
    pub setup: Vec<Vec<String>>,
    /// The tool manager for the repository's toolchain: `"mise"`.
    pub toolchain: Option<Toolchain>,
    /// Whether reviews get workspaces too (#170). The review loop opens its
    /// workspace whether or not this is set (#284).
    pub review: bool,
    /// Whether the planner gets a workspace too (#172), at the default
    /// branch.
    pub plan: bool,
    /// Named profiles; what they leave out comes from the default.
    pub profiles: BTreeMap<String, ProfileConfig>,
    /// Repository, as `owner/name`, to profile name.
    pub repositories: BTreeMap<String, String>,
    /// The sandbox host the `ssh` backend uses (#84).
    pub ssh: Option<SshConfig>,
    /// The cluster and namespace the `kubernetes` backend uses (#89).
    pub kubernetes: Option<KubernetesConfig>,
}

/// Where the `kubernetes` backend makes its Pods: a namespace of their own,
/// apart from Henk's, and the image they start from unless a profile names
/// another. Henk reaches the cluster with its own service account, or with
/// the kubeconfig the variable `kubeconfig_env` names; no token is in this
/// file (§8.4).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubernetesConfig {
    /// The sandbox namespace. Henk's service account needs a Role there and
    /// nowhere else (`deploy/kubernetes/`).
    pub namespace: String,
    /// The image every profile on this backend uses unless it names its own.
    pub image: Option<String>,
    /// A variable holding a kubeconfig's path, for running outside the
    /// cluster; unset, Henk uses the service account it runs as.
    pub kubeconfig_env: Option<String>,
    /// The user id the Pod runs as; never root.
    #[serde(default = "default_pod_user")]
    pub run_as_user: i64,
    /// A runtime class for the Pods, such as gVisor or Kata (#87).
    pub runtime_class: Option<String>,
    /// Node labels the Pods must land on, such as a sandbox node pool.
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
}

const fn default_pod_user() -> i64 {
    1000
}

impl KubernetesConfig {
    /// The table is usable, present when a profile needs it, and every
    /// profile on the backend has an image.
    fn check(kubernetes: Option<&Self>, workspace: &WorkspacePolicy) -> Result<(), ConfigError> {
        let refuse = |what: String| Err(ConfigError::Workspace(what));
        let on_pods: Vec<(&str, &Profile)> = workspace
            .named()
            .filter(|(_, p)| p.backend == BackendKind::Kubernetes)
            .collect();
        let Some(config) = kubernetes else {
            return if on_pods.is_empty() {
                Ok(())
            } else {
                refuse(
                    "a workspace profile uses the kubernetes backend, but [workspace.kubernetes] is missing"
                        .to_owned(),
                )
            };
        };
        if !is_dns_label(&config.namespace) {
            return refuse(format!(
                "workspace.kubernetes: namespace {:?} is not a Kubernetes namespace name",
                config.namespace
            ));
        }
        if config.run_as_user <= 0 {
            return refuse(
                "workspace.kubernetes: run_as_user must be a user other than root".to_owned(),
            );
        }
        if config.image.as_deref().is_some_and(|i| i.trim().is_empty()) {
            return refuse("workspace.kubernetes: image is empty".to_owned());
        }
        if config
            .kubeconfig_env
            .as_deref()
            .is_some_and(|v| v.trim().is_empty())
        {
            return refuse("workspace.kubernetes: kubeconfig_env names no variable".to_owned());
        }
        if let Some((name, _)) = on_pods
            .iter()
            .find(|(_, p)| p.image.is_none() && config.image.is_none())
        {
            return refuse(format!(
                "workspace profile {name} uses the kubernetes backend but has no image, and [workspace.kubernetes] names none"
            ));
        }
        Ok(())
    }

    /// The image `profile` starts from.
    #[must_use]
    pub fn image_for<'a>(&'a self, profile: &'a Profile) -> Option<&'a str> {
        profile.image.as_deref().or(self.image.as_deref())
    }
}

/// A DNS label as Kubernetes names namespaces: lowercase letters, digits
/// and `-`, starting and ending with a letter or digit, at most 63.
fn is_dns_label(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
}

/// The sandbox host of the `ssh` backend: where it is, who Henk signs in
/// as, where his key is and which host key to expect. No secret is in this
/// file (§8.4): the key's path comes from the variable `key_path_env` names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshConfig {
    /// Host name or address.
    pub host: String,
    /// SSH port.
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// The user Henk signs in as: root, which Henk's own requests need to
    /// make and remove each run's user, or one with passwordless sudo.
    #[serde(default = "default_ssh_user")]
    pub user: String,
    /// The variable holding the path of Henk's private key.
    #[serde(default = "default_ssh_key_path_env")]
    pub key_path_env: String,
    /// The host's public key as an OpenSSH line (`ssh-ed25519 AAAA...`),
    /// pinned: any other key is refused, and there is no trust on first use.
    pub host_key: String,
}

const fn default_ssh_port() -> u16 {
    22
}

fn default_ssh_user() -> String {
    "root".to_owned()
}

fn default_ssh_key_path_env() -> String {
    "HENK_SANDBOX_KEY_PATH".to_owned()
}

impl SshConfig {
    /// The sandbox host is usable, and present when a profile needs it.
    fn check(ssh: Option<&Self>, workspace: &WorkspacePolicy) -> Result<(), ConfigError> {
        match ssh {
            Some(ssh) => ssh.validate(),
            None if workspace
                .named()
                .any(|(_, profile)| profile.backend == BackendKind::Ssh) =>
            {
                Err(ConfigError::Workspace(
                    "a workspace profile uses the ssh backend, but [workspace.ssh] is missing"
                        .to_owned(),
                ))
            }
            None => Ok(()),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let refuse = |what: &str| Err(ConfigError::Workspace(format!("workspace.ssh: {what}")));
        if self.host.trim().is_empty() {
            return refuse("host is empty");
        }
        if self.user.trim().is_empty() {
            return refuse("user is empty");
        }
        if self.key_path_env.trim().is_empty() {
            return refuse("key_path_env names no variable");
        }
        if russh::keys::PublicKey::from_openssh(self.host_key.trim()).is_err() {
            return refuse("host_key is not an OpenSSH public key line");
        }
        Ok(())
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        let profile = Profile::default();
        Self {
            backend: profile.backend,
            image: profile.image,
            command_secs: profile.limits.command_secs,
            run_secs: profile.limits.run_secs,
            output_bytes: profile.limits.output_bytes,
            memory_mib: profile.limits.memory_mib,
            cpus: profile.limits.cpus,
            pids: profile.limits.pids,
            disk_mib: profile.limits.disk_mib,
            setup: profile.setup,
            toolchain: profile.toolchain,
            review: profile.review,
            plan: profile.plan,
            profiles: BTreeMap::new(),
            repositories: BTreeMap::new(),
            ssh: None,
            kubernetes: None,
        }
    }
}

/// A named workspace profile. Every field is optional and defaults to the
/// `[workspace]` value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    /// Where workspaces run.
    pub backend: Option<BackendKind>,
    /// The image a backend starts from.
    pub image: Option<String>,
    /// Seconds one command may run.
    pub command_secs: Option<u64>,
    /// Seconds all commands of one run may take together.
    pub run_secs: Option<u64>,
    /// Bytes of output kept per command.
    pub output_bytes: Option<u64>,
    /// Memory in MiB.
    pub memory_mib: Option<u64>,
    /// CPUs.
    pub cpus: Option<u64>,
    /// Processes.
    pub pids: Option<u64>,
    /// Disk in MiB.
    pub disk_mib: Option<u64>,
    /// Setup commands; replaces the default's when given.
    pub setup: Option<Vec<Vec<String>>>,
    /// The tool manager; replaces the default's when given.
    pub toolchain: Option<Toolchain>,
    /// Whether reviews get workspaces; the default's when not given.
    pub review: Option<bool>,
    /// Whether the planner gets a workspace; the default's when not given.
    pub plan: Option<bool>,
}

/// The workspace settings, checked.
struct WorkspaceParts {
    policy: WorkspacePolicy,
    ssh: Option<SshConfig>,
    kubernetes: Option<KubernetesConfig>,
}

impl WorkspaceConfig {
    /// The policy, the sandbox host and the cluster, checked together: a
    /// profile on the ssh backend needs `[workspace.ssh]`, one on the
    /// kubernetes backend `[workspace.kubernetes]`. The default profile's
    /// command limit is `[workspace].command_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Workspace`] when the policy is unusable.
    fn into_parts(mut self) -> Result<WorkspaceParts, ConfigError> {
        let ssh = self.ssh.take();
        let kubernetes = self.kubernetes.take();
        let policy = self.into_policy()?;
        SshConfig::check(ssh.as_ref(), &policy)?;
        KubernetesConfig::check(kubernetes.as_ref(), &policy)?;
        Ok(WorkspaceParts {
            policy,
            ssh,
            kubernetes,
        })
    }

    fn into_policy(self) -> Result<WorkspacePolicy, ConfigError> {
        let default = Profile {
            backend: self.backend,
            image: self.image,
            limits: Limits {
                command_secs: self.command_secs,
                run_secs: self.run_secs,
                output_bytes: self.output_bytes,
                memory_mib: self.memory_mib,
                cpus: self.cpus,
                pids: self.pids,
                disk_mib: self.disk_mib,
            },
            setup: self.setup,
            toolchain: self.toolchain,
            review: self.review,
            plan: self.plan,
        };
        let profiles = self
            .profiles
            .into_iter()
            .map(|(name, p)| {
                let base = &default;
                let profile = Profile {
                    backend: p.backend.unwrap_or(base.backend),
                    image: p.image.or_else(|| base.image.clone()),
                    limits: Limits {
                        command_secs: p.command_secs.unwrap_or(base.limits.command_secs),
                        run_secs: p.run_secs.unwrap_or(base.limits.run_secs),
                        output_bytes: p.output_bytes.unwrap_or(base.limits.output_bytes),
                        memory_mib: p.memory_mib.or(base.limits.memory_mib),
                        cpus: p.cpus.or(base.limits.cpus),
                        pids: p.pids.or(base.limits.pids),
                        disk_mib: p.disk_mib.or(base.limits.disk_mib),
                    },
                    setup: p.setup.unwrap_or_else(|| base.setup.clone()),
                    toolchain: p.toolchain.or(base.toolchain),
                    review: p.review.unwrap_or(base.review),
                    plan: p.plan.unwrap_or(base.plan),
                };
                (name, profile)
            })
            .collect();
        let policy = WorkspacePolicy {
            default,
            profiles,
            repositories: self.repositories,
        };
        policy.validate().map_err(ConfigError::Workspace)?;
        Ok(policy)
    }
}

fn default_change_budget() -> u32 {
    20
}
fn default_sub_issue_cap() -> u32 {
    5
}
fn default_plan_max_rounds() -> u32 {
    3
}
fn default_plan_run_timeout_secs() -> u64 {
    3600
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
    /// Their GitHub user id: a commit Henk pushes credits them with the
    /// noreply address of this account.
    #[serde(default)]
    pub github_id: Option<u64>,
    /// Their GitLab user id.
    #[serde(default)]
    pub gitlab_id: Option<u64>,
    /// The name their trailers use instead of the account's login. Set
    /// together with `commit_email`.
    #[serde(default)]
    pub commit_name: Option<String>,
    /// The email their trailers use instead of the noreply address.
    #[serde(default)]
    pub commit_email: Option<String>,
}

/// What a person's commit trailers are built from (§3.5): platform ids and
/// configured values, never a display name (§2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Committer {
    /// GitHub user id.
    pub github_id: Option<u64>,
    /// GitLab user id.
    pub gitlab_id: Option<u64>,
    /// A configured name and email, which wins over the noreply address.
    pub commit_as: Option<CommitPerson>,
}

impl PersonConfig {
    fn committer(&self) -> Result<Committer, ConfigError> {
        let refused = |why: String| ConfigError::Person(self.discord_id, why);
        let commit_as = match (&self.commit_name, &self.commit_email) {
            (None, None) => None,
            (Some(name), Some(email)) => Some(
                Email::parse(email)
                    .and_then(|email| CommitPerson::new(name, email))
                    .map_err(|e| refused(e.to_string()))?,
            ),
            _ => {
                return Err(refused(
                    "commit_name and commit_email are set together".to_owned(),
                ));
            }
        };
        Ok(Committer {
            github_id: self.github_id,
            gitlab_id: self.gitlab_id,
            commit_as,
        })
    }
}

/// Why a configuration is unusable.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `[mcp_server]` is not usable (#248).
    #[error("{0}")]
    McpServe(String),
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
    /// A review setting is unusable.
    #[error("{0}")]
    Review(String),
    /// A role, or a stage that needs it, is not configured as it must be.
    #[error("{0}")]
    Role(String),
    /// The dashboard setting is unusable.
    #[error("{0}")]
    Dashboard(String),
    /// The server setting is unusable.
    #[error("{0}")]
    Server(String),
    /// A `[[people]]` entry is unusable.
    #[error("people entry {0}: {1}")]
    Person(DiscordUserId, String),
    /// The database setting is unusable.
    #[error("{0}")]
    Database(String),
    /// The workspace setting is unusable.
    #[error("{0}")]
    Workspace(String),
    /// A skill or a reference to one is unusable.
    #[error("skills: {0}")]
    Skill(String),
}

/// The configuration, validated and turned into domain values.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The HTTP server.
    pub server: ServerConfig,
    /// Where run records live.
    pub database: DatabaseConfig,
    /// True when `database` came from the deprecated `server.database_path`.
    pub legacy_database_path: bool,
    /// Henk's own MCP server, when configured (#248).
    pub mcp_server: Option<McpServeConfig>,
    /// The web dashboard, when configured.
    pub dashboard: Option<DashboardConfig>,
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
    /// Reviews: the loop, its limits and what it leaves out.
    pub review: ReviewConfig,
    /// The agents Henk runs (#322).
    pub roles: RolesConfig,
    /// Planning.
    pub planning: Option<PlanningConfig>,
    /// Where a model's tools and the project's checks run, per repository.
    pub workspace: WorkspacePolicy,
    /// The sandbox host of the `ssh` backend, when configured.
    pub workspace_ssh: Option<SshConfig>,
    /// The `kubernetes` backend's cluster settings, when configured.
    pub workspace_kubernetes: Option<KubernetesConfig>,
    /// What commit trailers credit each known person with, by Discord id.
    pub committers: BTreeMap<DiscordUserId, Committer>,
    /// MCP servers by alias.
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// `[skills].dir` as written, when configured.
    pub skills_dir: Option<String>,
    /// The loaded skills. Empty until [`crate::skills::attach`] loads them.
    pub skills: std::sync::Arc<crate::skills::SkillCatalog>,
    /// Limits every model session shares.
    pub agent: AgentFileConfig,
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
        if self.address.is_some() {
            return Err(moved("[address]", "[roles.executor]"));
        }
        validate_roles(
            &self.roles,
            &self.models,
            &self.discord.team_lead_ids,
            &allowlist,
        )?;
        validate_review(&self.review, &self.roles)?;
        if let Some(planning) = &self.planning {
            validate_planning(planning, &self.roles, &self.discord.team_lead_ids)?;
        }
        validate_mcp_references(self.github.as_ref(), self.gitlab.as_ref(), &self.mcp)?;
        let skills_dir = self.skills.map(|s| s.dir);
        validate_skill_references(skills_dir.as_deref(), &self.roles)?;
        let committers = self
            .people
            .iter()
            .map(|person| Ok((person.discord_id, person.committer()?)))
            .collect::<Result<BTreeMap<_, _>, ConfigError>>()?;
        let legacy_database_path = self.server.database_path.is_some();
        let WorkspaceParts {
            policy: workspace,
            ssh: workspace_ssh,
            kubernetes: workspace_kubernetes,
        } = self.workspace.into_parts()?;
        let database = resolve_database(self.database, self.server.database_path.clone())?;
        validate_ways_in(self.dashboard.as_ref(), self.mcp_server.as_ref())?;
        if !(1..=3650).contains(&self.server.keep_events_days) {
            return Err(ConfigError::Server(
                "server.keep_events_days must be between 1 and 3650".to_owned(),
            ));
        }

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
            database,
            legacy_database_path,
            dashboard: self.dashboard,
            mcp_server: self.mcp_server,
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
            review: self.review,
            roles: self.roles,
            planning: self.planning,
            workspace,
            workspace_ssh,
            workspace_kubernetes,
            committers,
            mcp: self.mcp,
            skills_dir,
            skills: std::sync::Arc::default(),
            agent: self.agent,
        })
    }
}

/// Where records live: `[database]`, or the deprecated `server.database_path`.
fn resolve_database(
    database: Option<DatabaseConfig>,
    legacy_path: Option<String>,
) -> Result<DatabaseConfig, ConfigError> {
    let database = match (database, legacy_path) {
        (Some(_), Some(_)) => {
            return Err(ConfigError::Database(
                "set either [database] or server.database_path, not both".to_owned(),
            ));
        }
        (Some(database), None) => database,
        (None, Some(path)) => DatabaseConfig::Sqlite { path },
        (None, None) => DatabaseConfig::default(),
    };
    match &database {
        DatabaseConfig::Sqlite { path } if path.trim().is_empty() => {
            return Err(ConfigError::Database("database.path is empty".to_owned()));
        }
        DatabaseConfig::Postgres { url_env } if url_env.trim().is_empty() => {
            return Err(ConfigError::Database(
                "database.url_env is empty".to_owned(),
            ));
        }
        _ => {}
    }
    Ok(database)
}

/// A key that moved (#322), refused with where it went.
fn moved(old: &str, new: &str) -> ConfigError {
    ConfigError::Role(format!(
        "{old} was removed: it is {new} now. Roles are configured in [roles.planner], [roles.reviewer] and [roles.executor]; the README's \"Migrating to roles\" says how"
    ))
}

/// Each configured role: its model exists, its tools are within its
/// ceiling (#171) and include `load_skill` when it lists skills, and its
/// limits are above zero; the executor's push settings hold.
fn validate_roles(
    roles: &RolesConfig,
    models: &BTreeMap<String, ModelFileConfig>,
    team_leads: &[DiscordUserId],
    allowlist: &Allowlist,
) -> Result<(), ConfigError> {
    let agents = [
        roles.planner.as_ref().map(|r| {
            (
                Role::Planner,
                &r.model,
                r.tools.as_deref(),
                &r.skills,
                r.max_turns(Role::Planner),
                r.timeout_secs(Role::Planner),
            )
        }),
        roles.reviewer.as_ref().map(|r| {
            (
                Role::Reviewer,
                &r.model,
                r.tools.as_deref(),
                &r.skills,
                r.max_turns(Role::Reviewer),
                r.timeout_secs(Role::Reviewer),
            )
        }),
        roles.executor.as_ref().map(|r| {
            (
                Role::Executor,
                &r.model,
                r.tools.as_deref(),
                &r.skills,
                r.max_turns,
                r.timeout_secs,
            )
        }),
    ];
    for (role, model, tools, skills, max_turns, timeout_secs) in agents.into_iter().flatten() {
        if !models.contains_key(model) {
            return Err(ConfigError::UnknownModel {
                what: format!("roles.{role}"),
                model: model.clone(),
            });
        }
        let granted = role
            .narrow(tools)
            .map_err(|e| ConfigError::Role(e.to_string()))?;
        if !skills.is_empty() && !Role::loads_skills(&granted) {
            return Err(ConfigError::Role(format!(
                "[roles.{role}] lists skills, but its tools leave out load_skill: add load_skill to tools or remove the skills"
            )));
        }
        if max_turns == 0 || timeout_secs == 0 {
            return Err(ConfigError::Role(format!(
                "roles.{role}: max_turns and timeout_secs must be above zero"
            )));
        }
    }
    if let Some(executor) = &roles.executor {
        validate_executor(executor, team_leads, allowlist)?;
    }
    Ok(())
}

fn validate_executor(
    executor: &ExecutorConfig,
    team_leads: &[DiscordUserId],
    allowlist: &Allowlist,
) -> Result<(), ConfigError> {
    if !team_leads.contains(&executor.requester_id) {
        return Err(ConfigError::Role(format!(
            "roles.executor.requester_id {} is not in discord.team_lead_ids",
            executor.requester_id
        )));
    }
    if executor.max_changed_files == 0 {
        return Err(ConfigError::Role(
            "roles.executor.max_changed_files must be above zero".to_owned(),
        ));
    }
    if executor
        .check_commands
        .iter()
        .any(|argv| argv.first().is_none_or(|program| program.trim().is_empty()))
    {
        return Err(ConfigError::Role(
            "roles.executor.check_commands has an empty command".to_owned(),
        ));
    }
    executor
        .henk_identity()
        .map_err(|e| ConfigError::Role(format!("roles.executor.identity: {e}")))?;
    for key in executor.repositories.keys() {
        let allowed = [Platform::GitHub, Platform::GitLab]
            .into_iter()
            .any(|platform| {
                RepoRef::parse(platform, key).is_ok_and(|repo| allowlist.allows(&repo))
            });
        if !allowed {
            return Err(ConfigError::Role(format!(
                "roles.executor.repositories.{key:?} is not an owner/name repository on the allowlist"
            )));
        }
    }
    Ok(())
}

fn validate_dashboard(dashboard: &DashboardConfig) -> Result<(), ConfigError> {
    let problem = if dashboard.allowed_github_ids.is_empty() {
        Some("dashboard.allowed_github_ids is empty; nobody could sign in")
    } else if !(1..=168).contains(&dashboard.session_hours) {
        Some("dashboard.session_hours must be between 1 and 168")
    } else if [
        &dashboard.client_id_env,
        &dashboard.client_secret_env,
        &dashboard.session_key_env,
    ]
    .iter()
    .any(|name| name.trim().is_empty())
    {
        Some("a dashboard secret has no variable name")
    } else if ![&dashboard.github_web_base, &dashboard.github_api_base]
        .iter()
        .all(|url| is_github_base(url))
    {
        Some("dashboard GitHub addresses must be https, or http to a loopback address")
    } else {
        None
    };
    problem.map_or(Ok(()), |p| Err(ConfigError::Dashboard(p.to_owned())))
}

/// Whether `raw` may be a GitHub address the dashboard signs in through:
/// https to any host, or plain http only to a loopback IP literal (a local
/// test server). The URL is parsed so that hosts like
/// `127.0.0.1.evil.com` or `127.0.0.1@evil.com` are judged by their real host.
fn is_github_base(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        _ => false,
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

/// Planning pairs the planner with the reviewer (#328), so it needs both
/// roles, and its requester is a Team Lead.
fn validate_planning(
    planning: &PlanningConfig,
    roles: &RolesConfig,
    team_lead_ids: &[DiscordUserId],
) -> Result<(), ConfigError> {
    if let Some((old, new)) = planning.moved_key() {
        return Err(moved(old, new));
    }
    for (missing, role) in [
        (roles.planner.is_none(), "[roles.planner]"),
        (roles.reviewer.is_none(), "[roles.reviewer]"),
    ] {
        if missing {
            return Err(ConfigError::Role(format!("[planning] needs {role}")));
        }
    }
    // The planner and the reviewer hand points and answers to each other:
    // a reviewer without raise_point would let a plan through unreviewed,
    // and a planner without write_plan or answer_point never publishes or
    // answers.
    let sides = [
        roles
            .planner
            .as_ref()
            .map(|r| (Role::Planner, r.tools(Role::Planner))),
        roles
            .reviewer
            .as_ref()
            .map(|r| (Role::Reviewer, r.tools(Role::Reviewer))),
    ];
    for (role, granted) in sides.into_iter().flatten() {
        if let Some(tool) = role.missing_plan_handoff(&granted) {
            return Err(ConfigError::Role(format!(
                "[roles.{role}] tools leave out {tool}, which [planning] needs: add {tool} to tools"
            )));
        }
    }
    if planning.max_rounds == 0 {
        return Err(ConfigError::Role(
            "planning.max_rounds must be at least 1".to_owned(),
        ));
    }
    if !team_lead_ids.contains(&planning.requester_id) {
        return Err(ConfigError::PlanningRequester(planning.requester_id));
    }
    Ok(())
}

/// The review's own settings: no key of the lanes or the fact-check
/// (#309), its ignore patterns and its loop.
fn validate_review(review: &ReviewConfig, roles: &RolesConfig) -> Result<(), ConfigError> {
    if let Some(key) = review.removed_key() {
        return Err(ConfigError::Review(format!(
            "{key} was removed: a review is now the reviewer↔executor loop. Configure [review.loop], [roles.reviewer] and [roles.executor] instead; the README's \"Migrating from lanes\" says how"
        )));
    }
    if review.ignore.iter().any(|p| p.trim().is_empty()) {
        return Err(ConfigError::Review(
            "review.ignore has an empty pattern".to_owned(),
        ));
    }
    validate_loop(review.r#loop.as_ref(), roles)
}

/// The loop pairs the reviewer with the executor, so it needs both roles.
fn validate_loop(review_loop: Option<&LoopConfig>, roles: &RolesConfig) -> Result<(), ConfigError> {
    let Some(review_loop) = review_loop else {
        return Ok(());
    };
    if let Some((old, new)) = review_loop.moved_key() {
        return Err(moved(old, new));
    }
    for (missing, role) in [
        (roles.reviewer.is_none(), "[roles.reviewer]"),
        (roles.executor.is_none(), "[roles.executor]"),
    ] {
        if missing {
            return Err(ConfigError::Role(format!("[review.loop] needs {role}")));
        }
    }
    // The loop's two sides hand findings and verdicts to each other: a
    // role whose tools leave out its side of the handoff never ends a
    // round the way the loop waits for.
    let sides = [
        roles
            .reviewer
            .as_ref()
            .map(|r| (Role::Reviewer, r.tools(Role::Reviewer))),
        roles.executor.as_ref().map(|r| (Role::Executor, r.tools())),
    ];
    for (role, granted) in sides.into_iter().flatten() {
        if let Some(tool) = role.missing_handoff(&granted) {
            return Err(ConfigError::Role(format!(
                "[roles.{role}] tools leave out {tool}, which the review loop needs: add {tool} to tools"
            )));
        }
    }
    if review_loop.max_rounds == 0 {
        return Err(ConfigError::Review(
            "review.loop.max_rounds must be at least 1".to_owned(),
        ));
    }
    Ok(())
}

/// `; skills: a, b`, or nothing when there are none.
fn skills_note(skills: &[SkillName]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = skills.iter().map(SkillName::as_str).collect();
    format!("; skills: {}", names.join(", "))
}

/// Skills may be listed only with a `[skills]` folder to load them from.
/// Whether each listed skill exists is checked when they are loaded.
fn validate_skill_references(dir: Option<&str>, roles: &RolesConfig) -> Result<(), ConfigError> {
    if dir.is_some_and(|d| d.trim().is_empty()) {
        return Err(ConfigError::Skill("skills.dir is empty".to_owned()));
    }
    if dir.is_some() {
        return Ok(());
    }
    let listing = [
        roles.planner.as_ref().map(|r| ("planner", &r.skills)),
        roles.reviewer.as_ref().map(|r| ("reviewer", &r.skills)),
        roles.executor.as_ref().map(|r| ("executor", &r.skills)),
    ];
    if let Some((role, _)) = listing
        .into_iter()
        .flatten()
        .find(|(_, skills)| !skills.is_empty())
    {
        return Err(ConfigError::Skill(format!(
            "roles.{role} lists skills, but there is no [skills] section to load them from"
        )));
    }
    Ok(())
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
            prompt_cache: self.prompt_cache,
        })
    }
}

/// `a, b and c`.
fn and_list(items: &[&str]) -> String {
    match items.split_last() {
        None => String::new(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

impl Settings {
    /// Where a review waiting for a slot links to (#262): the dashboard's
    /// overview, which lists it as queued, or the run's own link when there
    /// is no dashboard. The run's page has nothing to show until it starts.
    #[must_use]
    pub fn queue_link(&self, run: &henk_domain::run::RunId) -> String {
        if self.dashboard.is_some() {
            format!(
                "{}/dashboard/",
                self.server.public_base_url.trim_end_matches('/')
            )
        } else {
            self.run_link(run)
        }
    }

    /// Why Henk's own process needs `git`, in words, one reason each; empty
    /// when it does not (#254). Henk checks out the reviewed commit, copies
    /// the planner's repository and clones to push the executor's fixes, on
    /// its own side, with the platform's credential (§8.4), whatever the
    /// workspace backend.
    #[must_use]
    pub fn needs_git(&self) -> Vec<String> {
        use henk_domain::workspace::EnvLane;
        let mut reasons = Vec::new();
        for (name, profile) in self.workspace.named() {
            if profile.serves(EnvLane::Review) {
                reasons.push(format!("review workspaces (profile {name})"));
            }
            if self.planning.is_some() && profile.serves(EnvLane::Plan) {
                reasons.push(format!("the planner's copy (profile {name})"));
            }
        }
        if self.roles.executor.is_some() {
            reasons.push("the executor's pushes".to_owned());
        }
        reasons
    }

    /// Public link of a run.
    #[must_use]
    pub fn run_link(&self, run: &henk_domain::run::RunId) -> String {
        format!(
            "{}/runs/{run}",
            self.server.public_base_url.trim_end_matches('/')
        )
    }

    /// The server, its secrets and the database, for [`Self::describe`].
    fn describe_server(&self, out: &mut String) {
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
        let legacy = if self.legacy_database_path {
            " (server.database_path is deprecated; use [database])"
        } else {
            ""
        };
        let _ = writeln!(out, "Database:        {}{legacy}", self.database.describe());
        let _ = writeln!(
            out,
            "Keep events:     {} days (runs are kept)",
            self.server.keep_events_days
        );
        if let Some(dashboard) = &self.dashboard {
            let _ = writeln!(
                out,
                "Dashboard:       /dashboard for {} GitHub account(s), sign-in via ${}",
                dashboard.allowed_github_ids.len(),
                dashboard.client_id_env
            );
        }
        if let Some(mcp) = &self.mcp_server {
            let tokens: Vec<String> = mcp
                .tokens
                .iter()
                .map(|t| format!("{} ({}, ${})", t.name, t.scope.as_str(), t.env))
                .collect();
            let _ = writeln!(
                out,
                "MCP server:      /mcp {}, tokens: {}",
                if mcp.enabled { "on" } else { "off" },
                if tokens.is_empty() {
                    "none".to_owned()
                } else {
                    tokens.join(", ")
                }
            );
        }
    }

    /// The workspace profiles, for [`Self::describe`], with a warning for
    /// every backend that is not isolated.
    fn describe_workspace(&self, out: &mut String) {
        let _ = writeln!(out, "Workspace:");
        let mut weak = Vec::new();
        for (name, profile) in self.workspace.named() {
            let limits = &profile.limits;
            let _ = write!(
                out,
                "  {name}: {}, {}s per command, {}s per run, {} bytes of output",
                profile.backend, limits.command_secs, limits.run_secs, limits.output_bytes
            );
            if let Some(image) = &profile.image {
                let _ = write!(out, ", image {image}");
            }
            if let Some(toolchain) = profile.toolchain {
                let _ = write!(out, ", toolchain {toolchain}");
            }
            match profile.setup.len() {
                0 => {}
                1 => out.push_str(", 1 setup step"),
                n => {
                    let _ = write!(out, ", {n} setup steps");
                }
            }
            if profile.review {
                out.push_str(", reviews in a workspace");
            }
            if profile.plan {
                out.push_str(", plans in a workspace");
            }
            for (what, value, unit) in [
                ("memory", limits.memory_mib, " MiB"),
                ("cpu", limits.cpus, ""),
                ("pids", limits.pids, ""),
                ("disk", limits.disk_mib, " MiB"),
            ] {
                if let Some(value) = value {
                    let _ = write!(out, ", {what} {value}{unit}");
                }
            }
            let users: Vec<&str> = self
                .workspace
                .repositories
                .iter()
                .filter(|(_, p)| *p == name)
                .map(|(r, _)| r.as_str())
                .collect();
            if !users.is_empty() {
                let _ = write!(out, "; for {}", users.join(", "));
            }
            out.push('\n');
            if !profile.backend.is_isolated() && !weak.contains(&profile.backend) {
                weak.push(profile.backend);
            }
        }
        if let Some(ssh) = &self.workspace_ssh {
            let _ = writeln!(
                out,
                "  sandbox host: {}@{}:{} (key from ${})",
                ssh.user, ssh.host, ssh.port, ssh.key_path_env
            );
        }
        if let Some(kube) = &self.workspace_kubernetes {
            let _ = writeln!(
                out,
                "  sandbox pods: namespace {}, image {}, as user {}{}{}; the number of processes is the node's limit",
                kube.namespace,
                kube.image.as_deref().unwrap_or("from each profile"),
                kube.run_as_user,
                kube.runtime_class
                    .as_deref()
                    .map_or_else(String::new, |c| format!(", runtime class {c}")),
                kube.kubeconfig_env.as_deref().map_or_else(
                    || ", in-cluster".to_owned(),
                    |v| format!(", kubeconfig from ${v}")
                ),
            );
        }
        for backend in weak {
            let unenforced = and_list(&Limits::unenforced_on(backend));
            let _ = match backend {
                BackendKind::Host => writeln!(
                    out,
                    "Warning: the host backend is not isolated; checks run as Henk's user, and {unenforced} limits are not enforced."
                ),
                BackendKind::Ssh => writeln!(
                    out,
                    "Warning: the ssh backend runs each run as its own user on {}; runs share its kernel, /tmp and network, and {unenforced} limits are not enforced.",
                    self.workspace_ssh
                        .as_ref()
                        .map_or("the sandbox host", |s| s.host.as_str())
                ),
                BackendKind::Kubernetes => Ok(()),
            };
        }
    }

    /// The roles, the stages that use them, and the skills.
    fn describe_agents(&self, out: &mut String) {
        self.describe_roles(out);
        if let Some(review_loop) = &self.review.r#loop {
            let _ = writeln!(
                out,
                "Review loop:     reviewer and executor; up to {} rounds, {}s in all",
                review_loop.max_rounds, review_loop.run_timeout_secs
            );
        } else {
            let _ = writeln!(out, "Reviews:         off (no [review.loop])");
        }
        if let Some(planning) = &self.planning {
            let _ = writeln!(
                out,
                "Planning:        planner and reviewer for Team Lead {}; {} changes, {} sub-issues; up to {} rounds, {}s in all",
                planning.requester_id,
                planning.change_budget,
                planning.sub_issue_cap,
                planning.max_rounds,
                planning.run_timeout_secs
            );
        }
        if let Some(executor) = &self.roles.executor {
            let _ = writeln!(
                out,
                "Pushes:          for Team Lead {}; {} files, {} checks",
                executor.requester_id,
                executor.max_changed_files,
                executor.check_commands.len()
            );
            self.describe_commits(executor, out);
        }
        self.describe_skills(out);
    }

    /// One line per configured role: its model, limits, tools when they
    /// differ from the defaults, skills, and whether it reaches the web.
    fn describe_roles(&self, out: &mut String) {
        let roles = [
            self.roles.planner.as_ref().map(|r| {
                (
                    Role::Planner,
                    &r.model,
                    r.max_turns(Role::Planner),
                    r.timeout_secs(Role::Planner),
                    r.tools(Role::Planner),
                    &r.skills,
                )
            }),
            self.roles.reviewer.as_ref().map(|r| {
                (
                    Role::Reviewer,
                    &r.model,
                    r.max_turns(Role::Reviewer),
                    r.timeout_secs(Role::Reviewer),
                    r.tools(Role::Reviewer),
                    &r.skills,
                )
            }),
            self.roles.executor.as_ref().map(|r| {
                (
                    Role::Executor,
                    &r.model,
                    r.max_turns,
                    r.timeout_secs,
                    r.tools(),
                    &r.skills,
                )
            }),
        ];
        for (role, model, turns, secs, tools, skills) in roles.into_iter().flatten() {
            let mut line = format!(
                "{:<17}model {model}; {turns} turns, {secs}s a turn",
                format!("Role {role}:")
            );
            if tools != role.default_tools() {
                let _ = write!(line, "; tools: {}", tools.join(", "));
            }
            line.push_str(&skills_note(skills));
            if Role::reaches_web(&tools) {
                line.push_str("; reaches the web");
            }
            let _ = writeln!(out, "{line}");
        }
    }

    fn describe_skills(&self, out: &mut String) {
        let Some(dir) = &self.skills.dir else {
            return;
        };
        let _ = writeln!(
            out,
            "Skills:          {} from {}",
            self.skills.skills.len(),
            dir.display()
        );
        for name in self.skills.skills.keys() {
            match self.skills.ignored.get(name) {
                Some(files) => {
                    let _ = writeln!(out, "  {name} (not read: {})", files.join(", "));
                }
                None => {
                    let _ = writeln!(out, "  {name}");
                }
            }
        }
    }

    /// Henk's commit identity and trailers, for [`Self::describe`], with a
    /// warning when a requester trailer is on but the requester has no
    /// account to credit.
    fn describe_commits(&self, address: &ExecutorConfig, out: &mut String) {
        let identity = match address.henk_identity() {
            Ok(Some(person)) => person.to_string(),
            Ok(None) => "the GitHub App's noreply address".to_owned(),
            Err(error) => error.to_string(),
        };
        let _ = writeln!(out, "Commits as:      {identity}");
        let _ = writeln!(
            out,
            "Trailers:        {}",
            describe_policy(address.trailers.over(TrailerPolicy::default()))
        );
        for key in address.repositories.keys() {
            // Resolved the way a run resolves it, so this shows what applies.
            if let Ok(repo) = RepoRef::parse(Platform::GitHub, key)
                .or_else(|_| RepoRef::parse(Platform::GitLab, key))
            {
                let policy = address.trailer_policy(&repo);
                let _ = writeln!(out, "  {key}: {}", describe_policy(policy));
            }
        }
        let credits_requester = address
            .policies()
            .any(|p| p.requester_coauthor || p.requester_signoff);
        let creditable = self.committers.get(&address.requester_id).is_some_and(|c| {
            c.github_id.is_some() || c.gitlab_id.is_some() || c.commit_as.is_some()
        });
        if credits_requester && !creditable {
            let _ = writeln!(
                out,
                "Warning:         requester {} has no github_id, gitlab_id or commit_email in [[people]]; their commits get no requester trailers",
                address.requester_id
            );
        }
    }

    /// Review limits, the repeat guard and what is not reviewed, for
    /// [`Self::describe`].
    fn describe_limits(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "Review limits:   {} at once",
            self.review.max_concurrent
        );
        let _ = writeln!(
            out,
            "Agent guard:     {}",
            match self.agent.max_repeated_calls {
                0 => "off".to_owned(),
                n => format!("{n} identical tool calls in a row, then the next is refused"),
            }
        );
        let _ = writeln!(
            out,
            "Tool calls:      every call on the run record, {}",
            match self.agent.record_argument_bytes {
                0 => "without its arguments".to_owned(),
                n => format!("with up to {n} bytes of its arguments"),
            }
        );
        let _ = writeln!(
            out,
            "Not reviewed:    {}",
            if self.review.ignore.is_empty() {
                "nothing ignored".to_owned()
            } else {
                self.review.ignore.join(", ")
            }
        );
    }

    /// A plain-text description of what Henk would work with.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        self.describe_server(&mut out);
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
        self.describe_limits(&mut out);
        self.describe_agents(&mut out);
        self.describe_workspace(&mut out);
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

fn describe_policy(policy: TrailerPolicy) -> String {
    let on = |yes: bool| if yes { "on" } else { "off" };
    format!(
        "Henk's sign-off {}, requester co-author {}, requester sign-off {}",
        on(policy.henk_signoff),
        on(policy.requester_coauthor),
        on(policy.requester_signoff)
    )
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
    fn events_are_kept_thirty_days_unless_configured() {
        let default = Config::parse(MINIMAL)
            .and_then(Config::into_settings)
            .unwrap();
        assert_eq!(default.server.keep_events_days, 30);
        assert!(
            default
                .describe()
                .contains("Keep events:     30 days (runs are kept)")
        );
        let week = format!("[server]\nkeep_events_days = 7\n{MINIMAL}");
        let settings = Config::parse(&week)
            .and_then(Config::into_settings)
            .unwrap();
        assert_eq!(settings.server.keep_events_days, 7);
        for days in [0, 3651] {
            let text = format!("[server]\nkeep_events_days = {days}\n{MINIMAL}");
            let error = Config::parse(&text)
                .and_then(Config::into_settings)
                .unwrap_err();
            assert!(matches!(error, ConfigError::Server(_)), "{days}: {error}");
        }
    }

    #[test]
    fn example_config_is_valid() {
        let settings = Config::parse(EXAMPLE)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(settings.review.r#loop.is_some());
        let roles = &settings.roles;
        assert_eq!(
            (
                roles.reviewer.as_ref().unwrap().model.as_str(),
                roles.executor.as_ref().unwrap().model.as_str(),
                roles.planner.as_ref().unwrap().model.as_str(),
            ),
            ("opus", "claude", "claude")
        );
        let described = settings.describe();
        for line in [
            "Review loop:     reviewer and executor; up to 50 rounds, 3600s in all",
            "Role planner:    model claude; 80 turns, 1200s a turn; reaches the web",
            "Role reviewer:   model opus; 40 turns, 600s a turn\n",
            "Role executor:   model claude; 40 turns, 600s a turn\n",
        ] {
            assert!(described.contains(line), "{line}\n{described}");
        }
        assert!(!described.contains("Review lanes:"), "{described}");
        assert!(settings.github.is_some());
        assert!(settings.gitlab.is_some());
        assert_eq!(settings.mcp.len(), 3);
        assert!(settings.models.contains_key("proxy-fast"));
        assert_eq!(
            settings.database,
            DatabaseConfig::Sqlite {
                path: "/var/lib/henk/henk.db".into()
            }
        );
        assert!(
            !settings.legacy_database_path,
            "the example uses [database]"
        );
        let scratch = RepoRef::parse(Platform::GitHub, "StephanMeijer/scratch-repo").unwrap();
        assert!(settings.allowlist.allows(&scratch));
        assert_eq!(
            settings.run_link(&henk_domain::run::RunId::parse("r-1").unwrap()),
            "https://henk.example.com/runs/r-1"
        );
    }

    fn database(extra: &str) -> Result<Settings, ConfigError> {
        Config::parse(&format!("{MINIMAL}{extra}")).and_then(Config::into_settings)
    }

    #[test]
    fn the_database_defaults_to_a_local_sqlite_file() {
        let settings = database("").unwrap();
        assert_eq!(
            settings.database,
            DatabaseConfig::Sqlite {
                path: "henk.db".into()
            }
        );
        assert!(!settings.legacy_database_path);
    }

    #[test]
    fn each_backend_parses() {
        let sqlite =
            database("[database]\nbackend = \"sqlite\"\npath = \"/var/lib/henk/henk.db\"\n")
                .unwrap();
        assert_eq!(
            sqlite.database,
            DatabaseConfig::Sqlite {
                path: "/var/lib/henk/henk.db".into()
            }
        );
        let postgres =
            database("[database]\nbackend = \"postgres\"\nurl_env = \"HENK_DATABASE_URL\"\n")
                .unwrap();
        assert_eq!(
            postgres.database,
            DatabaseConfig::Postgres {
                url_env: "HENK_DATABASE_URL".into()
            }
        );
        assert!(
            postgres
                .describe()
                .contains("PostgreSQL (URL from $HENK_DATABASE_URL)")
        );
    }

    #[test]
    fn the_old_database_path_still_works_and_says_it_is_deprecated() {
        let settings = database("[server]\ndatabase_path = \"old.db\"\n").unwrap();
        assert_eq!(
            settings.database,
            DatabaseConfig::Sqlite {
                path: "old.db".into()
            }
        );
        assert!(settings.legacy_database_path);
        assert!(
            settings
                .describe()
                .contains("server.database_path is deprecated")
        );
    }

    #[test]
    fn bad_database_settings_are_refused() {
        let both = "[server]\ndatabase_path = \"old.db\"\n[database]\nbackend = \"sqlite\"\n";
        assert!(matches!(database(both), Err(ConfigError::Database(_))));
        assert!(matches!(
            database("[database]\nbackend = \"mysql\"\n"),
            Err(ConfigError::Syntax(_))
        ));
        assert!(
            matches!(
                database("[database]\nbackend = \"postgres\"\n"),
                Err(ConfigError::Syntax(_)),
            ),
            "postgres needs url_env"
        );
        assert!(
            matches!(
                database("[database]\nbackend = \"postgres\"\nurl_env = \"X\"\npath = \"a.db\"\n"),
                Err(ConfigError::Syntax(_))
            ),
            "a sqlite field on postgres is refused"
        );
        assert!(matches!(
            database("[database]\nbackend = \"postgres\"\nurl_env = \" \"\n"),
            Err(ConfigError::Database(_))
        ));
    }

    #[test]
    fn container_example_is_valid() {
        let text = include_str!("../../../deploy/henk.container.example.toml");
        let settings = Config::parse(text)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!settings.legacy_database_path);
    }

    #[test]
    fn the_mcp_server_needs_named_unique_tokens_when_it_is_on() {
        assert!(
            database("").unwrap().mcp_server.is_none(),
            "off unless configured"
        );
        let token = |name: &str, scope: &str| {
            format!("[[mcp_server.tokens]]\nname = \"{name}\"\nenv = \"T\"\nscope = \"{scope}\"\n")
        };
        let on = database(&format!(
            "[mcp_server]\nenabled = true\n{}",
            token("claude-code", "write")
        ))
        .unwrap();
        let config = on.mcp_server.unwrap();
        assert!(config.enabled);
        assert_eq!(
            config.tokens.first().map(|t| t.scope),
            Some(McpScope::Write)
        );
        for (bad, why) in [
            ("[mcp_server]\nenabled = true\n".to_owned(), "at least one"),
            (
                format!("[mcp_server]\n{}", token("Claude", "read")),
                "lowercase",
            ),
            (
                format!(
                    "[mcp_server]\n{}{}",
                    token("a", "read"),
                    token("a", "write")
                ),
                "named twice",
            ),
            (
                format!(
                    "[mcp_server]\n{}{}",
                    token("a", "read"),
                    token("b", "write")
                ),
                "another token uses too",
            ),
        ] {
            let error = database(&bad).unwrap_err().to_string();
            assert!(error.contains(why), "{bad}: {error}");
        }
        assert!(
            database(&format!("[mcp_server]\n{}", token("a", "admin"))).is_err(),
            "unknown scope"
        );
    }

    #[test]
    fn git_is_needed_for_what_checks_out_a_repository_on_henk_s_side() {
        assert!(
            database("").unwrap().needs_git().is_empty(),
            "nothing checks out"
        );
        let ssh = format!(
            "[workspace.ssh]\nhost = \"sandbox.example\"\nhost_key = \"{}\"\n",
            host_key_line()
        );
        let reviewed = database(&format!(
            "[workspace.profiles.reviewed]\nbackend = \"ssh\"\nreview = true\n{ssh}"
        ))
        .unwrap();
        assert_eq!(
            reviewed.needs_git(),
            ["review workspaces (profile reviewed)"]
        );

        let address = Config::parse(&format!(
            "{MINIMAL}[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\nmodel = \"x\"\n[roles.executor]\nmodel = \"m\"\nrequester_id = 3\n"
        ))
        .and_then(Config::into_settings)
        .unwrap();
        assert_eq!(address.needs_git(), ["the executor's pushes"]);
    }

    #[test]
    fn the_executor_is_configured_and_checked() {
        let with_model = format!(
            "{MINIMAL}[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\nmodel = \"x\"\n"
        );
        let parse = |extra: &str| {
            Config::parse(&format!("{with_model}{extra}")).and_then(Config::into_settings)
        };
        let settings = parse(
            "[roles.executor]\nmodel = \"m\"\nrequester_id = 3\ncheck_commands = [[\"cargo\", \"test\"]]\n",
        )
        .unwrap();
        let executor = settings.roles.executor.as_ref().unwrap();
        assert_eq!((executor.max_turns, executor.timeout_secs), (40, 600));
        assert_eq!(executor.max_changed_files, 20);
        assert_eq!(executor.check_commands, [["cargo", "test"]]);
        assert_eq!(executor.tools(), Role::Executor.default_tools());
        assert!(
            settings
                .describe()
                .contains("Role executor:   model m; 40 turns, 600s a turn")
        );

        for (bad, why) in [
            (
                "[roles.executor]\nmodel = \"nope\"\nrequester_id = 3\n",
                "unknown model",
            ),
            (
                "[roles.executor]\nmodel = \"m\"\nrequester_id = 99\n",
                "not a Team Lead",
            ),
            (
                "[roles.executor]\nmodel = \"m\"\nrequester_id = 3\nmax_changed_files = 0\n",
                "zero limit",
            ),
            (
                "[roles.executor]\nmodel = \"m\"\nrequester_id = 3\ncheck_commands = [[]]\n",
                "empty command",
            ),
            (
                "[roles.executor]\nmodel = \"m\"\nrequester_id = 3\npush_force = true\n",
                "unknown field",
            ),
            (
                "[roles.executor]\nmodel = \"m\"\nrequester_id = 3\nmax_turns = 0\n",
                "zero turns",
            ),
            (
                "[roles.executor]\nmodel = \"m\"\nrequester_id = 3\ncheck_timeout_secs = 60\n",
                "the old check limit",
            ),
        ] {
            assert!(parse(bad).is_err(), "{why}");
        }
    }

    /// The example with every commented setting after `[roles.executor]` switched
    /// on: its identity, trailers and repositories, and the commented fields
    /// of `[[people]]`.
    fn example_with_address() -> String {
        let mut in_address = false;
        EXAMPLE
            .lines()
            .map(|line| {
                in_address |= line == "[roles.executor]";
                let setting = line.strip_prefix("# ").filter(|rest| {
                    let header = rest.starts_with('[') && rest.trim_end().ends_with(']');
                    header || rest.contains(" = ")
                });
                match setting {
                    Some(rest) if in_address => rest.to_owned(),
                    _ => line.to_owned(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_example_commit_settings_are_valid() {
        let settings = Config::parse(&example_with_address())
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let address = settings.roles.executor.as_ref().unwrap();
        let henk = address.henk_identity().unwrap().unwrap();
        assert_eq!(henk.to_string(), "Meneer Henk <henk@example.com>");
        let scratch = RepoRef::parse(Platform::GitHub, "StephanMeijer/scratch-repo").unwrap();
        assert!(address.trailer_policy(&scratch).requester_signoff);
        let stephan = settings.committers.values().next().unwrap();
        assert_eq!(stephan.github_id, Some(1_234_567));
        assert_eq!(
            stephan.commit_as.as_ref().unwrap().to_string(),
            "Stephan Meijer <stephan@example.com>"
        );
        let described = settings.describe();
        assert!(
            described.contains("Commits as:      Meneer Henk <henk@example.com>"),
            "{described}"
        );
        assert!(
            described.contains("  StephanMeijer/scratch-repo: Henk's sign-off on, requester co-author on, requester sign-off on"),
            "{described}"
        );
        assert!(!described.contains("no requester trailers"), "{described}");
    }

    const ADDRESS: &str = "[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\nmodel = \"x\"\n[[people]]\ndiscord_id = 3\nname = \"Lead\"\nrole = \"team lead\"\nPERSON[roles.executor]\nmodel = \"m\"\nrequester_id = 3\n";

    fn address(person: &str, extra: &str) -> Result<Settings, ConfigError> {
        let text = format!("{MINIMAL}{}{extra}", ADDRESS.replace("PERSON", person));
        Config::parse(&text).and_then(Config::into_settings)
    }

    #[test]
    fn the_loop_needs_the_reviewer_and_the_executor() {
        let reviewer = "[roles.reviewer]\nmodel = \"m\"\n";
        let settings = address("", &format!("[review.loop]\n{reviewer}")).unwrap();
        let review_loop = settings.review.r#loop.as_ref().unwrap();
        assert_eq!(
            (review_loop.max_rounds, review_loop.run_timeout_secs),
            (50, 3600)
        );
        let role = settings.roles.reviewer.as_ref().unwrap();
        assert_eq!(
            (
                role.max_turns(Role::Reviewer),
                role.timeout_secs(Role::Reviewer)
            ),
            (40, 600)
        );
        let unknown = address("", "[review.loop]\n[roles.reviewer]\nmodel = \"x\"\n").unwrap_err();
        assert!(
            matches!(&unknown, ConfigError::UnknownModel { what, model } if what == "roles.reviewer" && model == "x"),
            "{unknown}"
        );
        let none = address("", &format!("[review.loop]\nmax_rounds = 0\n{reviewer}")).unwrap_err();
        assert!(matches!(none, ConfigError::Review(_)), "{none}");
        let no_reviewer = address("", "[review.loop]\n").unwrap_err();
        assert_eq!(
            no_reviewer.to_string(),
            "[review.loop] needs [roles.reviewer]"
        );
        let models = ADDRESS.split("[[people]]").next().unwrap();
        let text = format!("{MINIMAL}{models}[review.loop]\n{reviewer}");
        let alone = Config::parse(&text)
            .and_then(Config::into_settings)
            .unwrap_err();
        assert_eq!(alone.to_string(), "[review.loop] needs [roles.executor]");
    }

    #[test]
    fn a_role_may_only_narrow_its_tools() {
        let reviewer = |tools: &str| {
            address(
                "",
                &format!("[roles.reviewer]\nmodel = \"m\"\ntools = {tools}\n"),
            )
        };
        let narrowed = reviewer(r#"["read_file", "report_finding", "finish_round"]"#).unwrap();
        let role = narrowed.roles.reviewer.as_ref().unwrap();
        assert_eq!(
            role.tools(Role::Reviewer),
            ["read_file", "report_finding", "finish_round"]
        );
        assert!(
            narrowed
                .describe()
                .contains("tools: read_file, report_finding, finish_round"),
            "{}",
            narrowed.describe()
        );
        let above = reviewer(r#"["edit_file"]"#).unwrap_err().to_string();
        assert_eq!(
            above,
            "[roles.reviewer] tools: the reviewer may not have edit_file"
        );
        let unknown = reviewer(r#"["teleport"]"#).unwrap_err().to_string();
        assert_eq!(
            unknown,
            "[roles.reviewer] tools: teleport is not a tool Henk has"
        );
        // Only the planner's session is built with web_fetch (#334).
        let web = reviewer(r#"["read_file", "web_fetch"]"#)
            .unwrap_err()
            .to_string();
        assert_eq!(
            web,
            "[roles.reviewer] tools: the reviewer may not have web_fetch"
        );
        let role = address("", "[roles.auditor]\nmodel = \"m\"\n").unwrap_err();
        assert!(matches!(role, ConfigError::Syntax(_)), "{role}");
    }

    /// The review loop needs each side's handoff tools (#334).
    #[test]
    fn the_loop_refuses_roles_without_their_handoff_tools() {
        let reviewer = |tools: &str| {
            address(
                "",
                &format!("[review.loop]\n[roles.reviewer]\nmodel = \"m\"\n{tools}"),
            )
        };
        assert_eq!(
            reviewer("tools = [\"read_file\", \"finish_round\"]\n")
                .unwrap_err()
                .to_string(),
            "[roles.reviewer] tools leave out report_finding, which the review loop needs: add report_finding to tools"
        );
        assert_eq!(
            reviewer("tools = [\"read_file\", \"report_finding\"]\n")
                .unwrap_err()
                .to_string(),
            "[roles.reviewer] tools leave out finish_round, which the review loop needs: add finish_round to tools"
        );
        assert!(
            reviewer("tools = [\"read_file\", \"report_finding\", \"finish_round\"]\n").is_ok()
        );
        let executor = |tools: &str| {
            address(
                "",
                &format!("tools = {tools}\n[review.loop]\n[roles.reviewer]\nmodel = \"m\"\n"),
            )
        };
        assert_eq!(
            executor(r#"["edit_file", "give_verdict"]"#)
                .unwrap_err()
                .to_string(),
            "[roles.executor] tools leave out list_findings, which the review loop needs: add list_findings to tools"
        );
        assert_eq!(
            executor(r#"["edit_file", "list_findings"]"#)
                .unwrap_err()
                .to_string(),
            "[roles.executor] tools leave out give_verdict, which the review loop needs: add give_verdict to tools"
        );
        assert!(executor(r#"["edit_file", "list_findings", "give_verdict"]"#).is_ok());
        // Without the loop, the reviewer's list is its own business.
        assert!(
            address(
                "",
                "[roles.reviewer]\nmodel = \"m\"\ntools = [\"read_file\"]\n"
            )
            .is_ok()
        );
    }

    /// A role told of skills must be able to load them (#334).
    #[test]
    fn a_role_with_skills_keeps_load_skill() {
        let reviewer = |tools: &str| {
            address(
                "",
                &format!(
                    "[skills]\ndir = \"skills\"\n[roles.reviewer]\nmodel = \"m\"\nskills = [\"rust-errors\"]\n{tools}"
                ),
            )
        };
        let without = reviewer("tools = [\"read_file\", \"report_finding\"]\n")
            .unwrap_err()
            .to_string();
        assert_eq!(
            without,
            "[roles.reviewer] lists skills, but its tools leave out load_skill: add load_skill to tools or remove the skills"
        );
        assert!(reviewer("tools = [\"read_file\", \"load_skill\"]\n").is_ok());
        assert!(reviewer("").is_ok(), "the defaults have load_skill");
    }

    /// The keys that moved to the roles (#322): a file that still has one
    /// fails with where it went.
    #[test]
    fn moved_keys_name_their_role() {
        for (extra, old, new) in [
            (
                "[address]\nmodel = \"m\"\n",
                "[address]",
                "[roles.executor]",
            ),
            (
                "[review.loop]\nreviewer = \"m\"\n",
                "[review.loop].reviewer",
                "[roles.reviewer].model",
            ),
            (
                "[review.loop]\nfixer = \"m\"\n",
                "[review.loop].fixer",
                "[roles.executor].model",
            ),
            (
                "[review.loop]\nround_timeout_secs = 600\n",
                "[review.loop].round_timeout_secs",
                "timeout_secs on [roles.reviewer] and [roles.executor]",
            ),
            (
                "[review.loop]\nround_max_turns = 40\n",
                "[review.loop].round_max_turns",
                "max_turns on [roles.reviewer] and [roles.executor]",
            ),
            (
                "[planning]\nrequester_id = 3\nmodel = \"m\"\n",
                "[planning].model",
                "[roles.planner].model",
            ),
            (
                "[planning]\nrequester_id = 3\ntimeout_secs = 600\n",
                "[planning].timeout_secs",
                "[roles.planner].timeout_secs",
            ),
            (
                "[planning]\nrequester_id = 3\nmax_turns = 80\n",
                "[planning].max_turns",
                "[roles.planner].max_turns",
            ),
            (
                "[planning]\nrequester_id = 3\nskills = []\n",
                "[planning].skills",
                "[roles.planner].skills",
            ),
        ] {
            let models = ADDRESS.split("[[people]]").next().unwrap();
            let error = Config::parse(&format!("{MINIMAL}{models}{extra}"))
                .and_then(Config::into_settings)
                .unwrap_err();
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("{old} was removed: it is {new} now.")),
                "{message}"
            );
            assert!(message.contains("Migrating to roles"), "{message}");
            assert!(henk_domain::text::is_in_style(&message), "{message}");
        }
    }

    #[test]
    fn planning_needs_the_planner_and_the_reviewer() {
        let models = ADDRESS.split("[[people]]").next().unwrap();
        let planning = |extra: &str| {
            Config::parse(&format!(
                "{MINIMAL}{models}[planning]\nrequester_id = 3\n{extra}"
            ))
            .and_then(Config::into_settings)
        };
        let planner = "[roles.planner]\nmodel = \"m\"\n";
        let reviewer = "[roles.reviewer]\nmodel = \"m\"\n";
        let error = planning(reviewer).unwrap_err();
        assert_eq!(error.to_string(), "[planning] needs [roles.planner]");
        let error = planning(planner).unwrap_err();
        assert_eq!(error.to_string(), "[planning] needs [roles.reviewer]");
        for (extra, (role, tool)) in [
            (
                format!(
                    "{planner}{reviewer}tools = [\"read_file\", \"report_finding\", \"finish_round\"]\n"
                ),
                ("reviewer", "raise_point"),
            ),
            (
                format!("{planner}{reviewer}tools = [\"read_file\", \"raise_point\"]\n"),
                ("reviewer", "finish_round"),
            ),
            (
                format!("{reviewer}{planner}tools = [\"read_file\", \"answer_point\"]\n"),
                ("planner", "write_plan"),
            ),
            (
                format!("{reviewer}{planner}tools = [\"read_file\", \"write_plan\"]\n"),
                ("planner", "answer_point"),
            ),
        ] {
            let message = planning(&extra).unwrap_err().to_string();
            assert_eq!(
                message,
                format!(
                    "[roles.{role}] tools leave out {tool}, which [planning] needs: add {tool} to tools"
                )
            );
            assert!(henk_domain::text::is_in_style(&message), "{message}");
        }
        assert!(
            planning(&format!(
                "{planner}tools = [\"read_file\", \"write_plan\", \"answer_point\"]\n{reviewer}tools = [\"read_file\", \"raise_point\", \"finish_round\"]\n"
            ))
            .is_ok()
        );
        let error = planning(&format!("max_rounds = 0\n{planner}{reviewer}")).unwrap_err();
        assert_eq!(error.to_string(), "planning.max_rounds must be at least 1");
        let settings = planning(&format!("{planner}{reviewer}")).unwrap();
        let config = settings.planning.as_ref().unwrap();
        assert_eq!((config.max_rounds, config.run_timeout_secs), (3, 3600));
        let planner = settings.roles.planner.as_ref().unwrap();
        assert_eq!(
            (
                planner.max_turns(Role::Planner),
                planner.timeout_secs(Role::Planner)
            ),
            (80, 1200)
        );
    }

    #[test]
    fn trailers_default_and_resolve_per_repository() {
        let settings = address(
            "github_id = 77\n",
            "[roles.executor.trailers]\nhenk_signoff = false\n[roles.executor.repositories.\"docspec/dco\"]\nrequester_signoff = true\n",
        )
        .unwrap();
        let config = settings.roles.executor.as_ref().unwrap();
        assert_eq!(config.henk_identity().unwrap(), None);
        let other = RepoRef::parse(Platform::GitHub, "docspec/other").unwrap();
        assert_eq!(
            config.trailer_policy(&other),
            TrailerPolicy {
                henk_signoff: false,
                requester_coauthor: true,
                requester_signoff: false,
            }
        );
        let dco = RepoRef::parse(Platform::GitHub, "DocSpec/DCO").unwrap();
        assert_eq!(
            config.trailer_policy(&dco),
            TrailerPolicy {
                henk_signoff: false,
                requester_coauthor: true,
                requester_signoff: true,
            },
            "the repository's switch over [roles.executor.trailers], case aside"
        );
        let described = settings.describe();
        assert!(
            described.contains("Commits as:      the GitHub App's noreply address"),
            "{described}"
        );

        let plain = address("", "").unwrap();
        assert_eq!(
            plain
                .roles
                .executor
                .as_ref()
                .unwrap()
                .trailer_policy(&other),
            TrailerPolicy::default()
        );
    }

    #[test]
    fn a_requester_without_an_account_is_warned_about() {
        let warning = "Warning:         requester 3 has no github_id";
        let described = address("", "").unwrap().describe();
        assert!(described.contains(warning), "{described}");
        let off = address(
            "",
            "[roles.executor.trailers]\nrequester_coauthor = false\n",
        )
        .unwrap()
        .describe();
        assert!(!off.contains(warning), "nothing to credit: {off}");
        let on_somewhere = address(
            "",
            "[roles.executor.trailers]\nrequester_coauthor = false\n[roles.executor.repositories.\"docspec/r\"]\nrequester_signoff = true\n",
        )
        .unwrap()
        .describe();
        assert!(on_somewhere.contains(warning), "{on_somewhere}");
        for person in [
            "github_id = 77\n",
            "gitlab_id = 77\n",
            "commit_name = \"Lead\"\ncommit_email = \"lead@example.com\"\n",
        ] {
            let described = address(person, "").unwrap().describe();
            assert!(!described.contains(warning), "{person}: {described}");
        }
    }

    #[test]
    fn bad_commit_identities_and_repositories_are_refused() {
        for (person, extra, why) in [
            (
                "",
                "[roles.executor.identity]\nname = \"Henk\"\nemail = \"not-an-address\"\n",
                "bad identity email",
            ),
            (
                "",
                "[roles.executor.identity]\nname = \"Henk <h@x.nl>\"\nemail = \"h@x.nl\"\n",
                "bad identity name",
            ),
            (
                "",
                "[roles.executor.identity]\nname = \"Henk\"\n",
                "identity without email",
            ),
            (
                "commit_name = \"Lead\"\ncommit_email = \"lead@localhost\"\n",
                "",
                "person email without a domain dot",
            ),
            (
                "commit_name = \"Lead\"\ncommit_email = \"Lead <lead@example.com>\"\n",
                "",
                "person email with a name",
            ),
            (
                "commit_name = \"a>b\"\ncommit_email = \"lead@example.com\"\n",
                "",
                "person name with a bracket",
            ),
            (
                "commit_email = \"lead@example.com\"\n",
                "",
                "email without name",
            ),
            (
                "",
                "[roles.executor.repositories.\"elsewhere/r\"]\nrequester_signoff = true\n",
                "repository not on the allowlist",
            ),
            (
                "",
                "[roles.executor.repositories.\"docspec\"]\nrequester_signoff = true\n",
                "not owner/name",
            ),
            (
                "",
                "[roles.executor.repositories.\"docspec/r\"]\nrequester_sign_off = true\n",
                "unknown switch",
            ),
        ] {
            assert!(address(person, extra).is_err(), "{why}");
        }
        assert!(matches!(
            address("commit_email = \"x\"\ncommit_name = \"y\"\n", ""),
            Err(ConfigError::Person(id, _)) if id.get() == 3
        ));
    }

    #[test]
    fn workspace_is_configured_and_checked() {
        let defaults = database("").unwrap();
        assert_eq!(defaults.workspace, WorkspacePolicy::default());

        let settings = database(
            "[workspace]\ncommand_secs = 300\noutput_bytes = 4096\n[workspace.profiles.slow]\ncommand_secs = 3600\n[workspace.repositories]\n\"docspec/big\" = \"slow\"\n",
        )
        .unwrap();
        let slow = settings.workspace.profile_for("docspec/big");
        assert_eq!(slow.limits.command_secs, 3600);
        assert_eq!(slow.limits.output_bytes, 4096, "inherited from [workspace]");
        let other = settings.workspace.profile_for("docspec/app");
        assert_eq!(other.limits.command_secs, 300);

        for (bad, why) in [
            (
                "[workspace.repositories]\n\"docspec/big\" = \"nope\"\n",
                "unknown profile",
            ),
            ("[workspace]\ncommand_secs = 0\n", "zero limit"),
            (
                "[workspace.profiles.p]\npids = 0\n",
                "zero limit in a profile",
            ),
            ("[workspace]\nimage = \"rust:1\"\n", "an image on host"),
        ] {
            assert!(
                matches!(database(bad), Err(ConfigError::Workspace(_))),
                "{why}"
            );
        }
        for (bad, why) in [
            ("[workspace]\nsandbox = true\n", "unknown field"),
            (
                "[workspace]\nbackend = \"fake\"\n",
                "fake is for tests only",
            ),
            (
                "[workspace.profiles.p]\nnetwork = true\n",
                "unknown profile field",
            ),
        ] {
            assert!(
                matches!(database(bad), Err(ConfigError::Syntax(_))),
                "{why}"
            );
        }
    }

    #[test]
    fn setup_steps_and_the_toolchain_are_configured_per_profile() {
        let settings = database(
            "[workspace]\ntoolchain = \"mise\"\nsetup = [[\"make\", \"deps\"], [\"npm\", \"ci\"]]\n[workspace.profiles.bare]\nsetup = []\n[workspace.profiles.same]\ncommand_secs = 60\n",
        )
        .unwrap();
        let policy = &settings.workspace;
        assert_eq!(policy.default.toolchain, Some(Toolchain::Mise));
        assert_eq!(policy.default.setup, [["make", "deps"], ["npm", "ci"]]);
        let (bare, same) = (
            policy.profiles.get("bare").unwrap(),
            policy.profiles.get("same").unwrap(),
        );
        assert!(bare.setup.is_empty(), "a profile can drop the steps");
        assert_eq!(
            bare.toolchain,
            Some(Toolchain::Mise),
            "and keeps what it leaves out"
        );
        assert_eq!(same.setup, policy.default.setup);
        let text = settings.describe();
        assert!(
            text.contains("  default: host, 600s per command, 1800s per run, 20480 bytes of output, toolchain mise, 2 setup steps"),
            "{text}"
        );
        for bad in [
            "[workspace]\nsetup = [[]]\n",
            "[workspace]\nsetup = [[\" \"]]\n",
            "[workspace]\ntoolchain = \"asdf\"\n",
        ] {
            assert!(database(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reviews_get_workspaces_only_where_a_profile_turns_them_on() {
        let ssh = format!(
            "[workspace.ssh]\nhost = \"sandbox.example\"\nhost_key = \"{}\"\n",
            host_key_line()
        );
        let settings = database(&format!(
            "[workspace.profiles.reviewed]\nbackend = \"ssh\"\nreview = true\n[workspace.profiles.inherits]\ncommand_secs = 60\n{ssh}"
        ))
        .unwrap();
        let policy = &settings.workspace;
        assert!(!policy.default.review, "off by default");
        assert!(policy.profiles.get("reviewed").unwrap().review);
        assert!(!policy.profiles.get("inherits").unwrap().review);
        let text = settings.describe();
        assert!(
            text.contains("  reviewed: ssh, 600s per command, 1800s per run, 20480 bytes of output, reviews in a workspace"),
            "{text}"
        );
        assert!(
            !text.contains(
                "  default: host, 600s per command, 1800s per run, 20480 bytes of output, reviews"
            ),
            "{text}"
        );

        let all = database(&format!("[workspace]\nbackend = \"ssh\"\nreview = true\n[workspace.profiles.quiet]\nreview = false\n[workspace.profiles.loud]\n{ssh}")).unwrap();
        assert!(all.workspace.default.review);
        assert!(!all.workspace.profiles.get("quiet").unwrap().review);
        assert!(
            all.workspace.profiles.get("loud").unwrap().review,
            "inherited"
        );
    }

    #[test]
    fn a_review_workspace_on_the_host_backend_is_refused() {
        for (bad, profile) in [
            ("[workspace]\nreview = true\n", "default"),
            ("[workspace.profiles.p]\nreview = true\n", "p"),
            (
                "[workspace]\nreview = true\n[workspace.profiles.p]\nbackend = \"ssh\"\n",
                "default",
            ),
        ] {
            let error = database(bad).unwrap_err();
            assert!(matches!(error, ConfigError::Workspace(_)), "{bad}: {error}");
            let text = error.to_string();
            assert!(
                text.contains(&format!(
                    "workspace profile {profile}: review = true needs a backend apart from Henk"
                )),
                "{bad}: {text}"
            );
        }
    }

    #[test]
    fn the_planner_gets_a_workspace_where_a_profile_on_a_sandbox_says_so() {
        let ssh = format!(
            "[workspace.ssh]\nhost = \"sandbox.example\"\nhost_key = \"{}\"\n",
            host_key_line()
        );
        let settings = database(&format!(
            "[workspace.profiles.planned]\nbackend = \"ssh\"\nplan = true\n[workspace.profiles.inherits]\nbackend = \"ssh\"\n{ssh}"
        ))
        .unwrap();
        let policy = &settings.workspace;
        assert!(!policy.default.plan, "off by default");
        assert!(policy.profiles.get("planned").unwrap().plan);
        assert!(!policy.profiles.get("inherits").unwrap().plan);
        let text = settings.describe();
        assert!(text.contains(", plans in a workspace"), "{text}");
        let refused = database("[workspace]\nplan = true\n")
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("plan = true needs a backend apart from Henk"),
            "{refused}"
        );
    }

    #[test]
    fn the_kubernetes_backend_needs_its_namespace_and_an_image() {
        let pods = "[workspace]\nbackend = \"kubernetes\"\nreview = true\n";
        let refused = database(pods).unwrap_err().to_string();
        assert!(
            refused.contains("[workspace.kubernetes] is missing"),
            "{refused}"
        );
        let no_image = database(&format!(
            "{pods}[workspace.kubernetes]\nnamespace = \"henk-sandbox\"\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(no_image.contains("has no image"), "{no_image}");

        let settings = database(&format!(
            "{pods}[workspace.profiles.big]\nimage = \"big:2\"\nmemory_mib = 4096\n[workspace.kubernetes]\nnamespace = \"henk-sandbox\"\nimage = \"sandbox:1\"\nruntime_class = \"gvisor\"\n"
        ))
        .unwrap();
        let config = settings.workspace_kubernetes.as_ref().unwrap();
        assert_eq!(
            (config.namespace.as_str(), config.run_as_user),
            ("henk-sandbox", 1000)
        );
        let big = settings.workspace.profiles.get("big").unwrap();
        assert_eq!(
            config.image_for(big),
            Some("big:2"),
            "a profile's own image"
        );
        assert_eq!(
            config.image_for(&settings.workspace.default),
            Some("sandbox:1")
        );
        let text = settings.describe();
        assert!(
            text.contains("  default: kubernetes, 600s per command"),
            "{text}"
        );
        assert!(
            text.contains(", image big:2, reviews in a workspace, memory 4096 MiB"),
            "{text}"
        );
        assert!(
            text.contains("  sandbox pods: namespace henk-sandbox, image sandbox:1, as user 1000, runtime class gvisor, in-cluster; the number of processes is the node's limit"),
            "{text}"
        );
        assert!(!text.contains("Warning: the kubernetes"), "{text}");

        for bad in [
            "namespace = \"Henk_Sandbox\"\nimage = \"x\"\n",
            "namespace = \"ok\"\nimage = \"x\"\nrun_as_user = 0\n",
            "namespace = \"ok\"\nimage = \" \"\n",
            "namespace = \"ok\"\nimage = \"x\"\nkubeconfig_env = \"\"\n",
            "namespace = \"ok\"\nimage = \"x\"\ncluster_admin = true\n",
        ] {
            assert!(
                database(&format!("{pods}[workspace.kubernetes]\n{bad}")).is_err(),
                "{bad}"
            );
        }
    }

    fn host_key_line() -> String {
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[1; 32]),
        );
        key.public_key().to_openssh().unwrap()
    }

    #[test]
    fn the_ssh_backend_needs_its_sandbox_host_and_says_what_it_shares() {
        let ssh = format!(
            "[workspace.profiles.sandbox]\nbackend = \"ssh\"\n[workspace.repositories]\n\"docspec/app\" = \"sandbox\"\n[workspace.ssh]\nhost = \"sandbox.example\"\nhost_key = \"{}\"\n",
            host_key_line()
        );
        let settings = database(&format!("[workspace]\n{ssh}")).unwrap();
        let config = settings.workspace_ssh.as_ref().unwrap();
        assert_eq!(
            (
                config.port,
                config.user.as_str(),
                config.key_path_env.as_str()
            ),
            (22, "root", "HENK_SANDBOX_KEY_PATH")
        );
        let text = settings.describe();
        assert!(text.contains("  sandbox: ssh, 600s per command"), "{text}");
        assert!(
            text.contains(
                "  sandbox host: root@sandbox.example:22 (key from $HENK_SANDBOX_KEY_PATH)"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "Warning: the ssh backend runs each run as its own user on sandbox.example; runs share its kernel, /tmp and network, and memory, cpu, pids and disk limits are not enforced."
            ),
            "{text}"
        );
        assert!(henk_domain::text::is_in_style(&text), "{text}");

        let missing = database("[workspace.profiles.sandbox]\nbackend = \"ssh\"\n").unwrap_err();
        assert!(
            missing.to_string().contains("[workspace.ssh] is missing"),
            "{missing}"
        );
        for (bad, why) in [
            ("host = \"\"\nhost_key = \"x\"", "host is empty"),
            (
                "host = \"h\"\nhost_key = \"not a key\"",
                "host_key is not an OpenSSH public key line",
            ),
            (
                "host = \"h\"\nhost_key = \"x\"\npassword = \"no\"",
                "unknown field",
            ),
        ] {
            let error = database(&format!("[workspace.ssh]\n{bad}\n")).unwrap_err();
            assert!(error.to_string().contains(why), "{bad}: {error}");
        }
    }

    #[test]
    fn describe_shows_the_workspace_and_warns_that_host_is_not_isolated() {
        let settings = database(
            "[workspace]\nmemory_mib = 512\n[workspace.profiles.slow]\ncommand_secs = 3600\n[workspace.repositories]\n\"docspec/big\" = \"slow\"\n",
        )
        .unwrap();
        let text = settings.describe();
        assert!(
            text.contains(
                "Workspace:\n  default: host, 600s per command, 1800s per run, 20480 bytes of output, memory 512 MiB\n  slow: host, 3600s per command"
            ),
            "{text}"
        );
        assert!(text.contains("; for docspec/big"), "{text}");
        assert!(
            text.contains(
                "Warning: the host backend is not isolated; checks run as Henk's user, and memory, cpu, pids and disk limits are not enforced."
            ),
            "{text}"
        );
        assert_eq!(text.matches("Warning:").count(), 1, "{text}");
        assert!(henk_domain::text::is_in_style(&text), "{text}");
    }

    #[test]
    fn the_dashboard_is_optional_and_checked() {
        assert!(database("").unwrap().dashboard.is_none());
        let settings = database("[dashboard]\nallowed_github_ids = [1234]\n").unwrap();
        let dashboard = settings.dashboard.as_ref().unwrap();
        assert_eq!(dashboard.session_hours, 12);
        assert_eq!(dashboard.client_secret_env, "HENK_DASHBOARD_CLIENT_SECRET");
        assert!(
            settings
                .describe()
                .contains("Dashboard:       /dashboard for 1 GitHub account(s)")
        );
        for (bad, why) in [
            ("[dashboard]\nallowed_github_ids = []\n", "nobody"),
            (
                "[dashboard]\nallowed_github_ids = [1]\nsession_hours = 0\n",
                "zero hours",
            ),
            (
                "[dashboard]\nallowed_github_ids = [1]\nsession_key_env = \" \"\n",
                "no name",
            ),
            (
                "[dashboard]\nallowed_github_ids = [1]\ngithub_web_base = \"http://github.com\"\n",
                "plain http",
            ),
            (
                "[dashboard]\nallowed_github_ids = [\"alice\"]\n",
                "a login, not an id",
            ),
        ] {
            assert!(database(bad).is_err(), "{why}");
        }
    }

    #[test]
    fn dashboard_github_addresses_are_judged_by_their_parsed_host() {
        for good in [
            "https://github.com",
            "https://api.github.com/",
            "https://ghe.example.test/api/v3",
            "http://127.0.0.1",
            "http://127.0.0.1:8080/",
            "http://127.0.0.2:9000",
            "http://[::1]:8080",
        ] {
            assert!(is_github_base(good), "{good}");
        }
        for bad in [
            "http://127.0.0.1.evil.com",
            "http://127.0.0.1.evil.com:8080/",
            "http://127.0.0.1@evil.com",
            "http://127.0.0.1:80@evil.com",
            "http://localhost:8080",
            "http://github.com",
            "http://10.0.0.1",
            "ftp://127.0.0.1",
            "https://",
            "127.0.0.1:8080",
            "",
        ] {
            assert!(!is_github_base(bad), "{bad}");
        }
        assert!(
            database(
                "[dashboard]\nallowed_github_ids = [1]\ngithub_api_base = \"http://127.0.0.1.evil.com\"\n"
            )
            .is_err()
        );
        assert!(
            database(
                "[dashboard]\nallowed_github_ids = [1]\ngithub_api_base = \"http://127.0.0.1:9000\"\n"
            )
            .is_ok()
        );
    }

    #[test]
    fn minimal_config_is_valid() {
        let settings = Config::parse(MINIMAL)
            .and_then(Config::into_settings)
            .unwrap();
        assert!(settings.review.r#loop.is_none());
        assert_eq!(settings.server.bind, "127.0.0.1:8080");
        assert!(
            settings
                .describe()
                .contains("Reviews:         off (no [review.loop])"),
            "{}",
            settings.describe()
        );
    }

    /// The keys of the lanes and the fact-check are gone (#309): a file
    /// that still has one fails with what replaced them.
    #[test]
    fn lane_and_fact_check_keys_name_the_review_loop() {
        let model = "[models.m]\nprovider = \"anthropic\"\nbase_url = \"https://x\"\napi_key_env = \"K\"\nmodel = \"c\"\n";
        for (section, key) in [
            (
                "[review]\nlanes = [{ name = \"a\", model = \"m\" }]\n",
                "[review].lanes",
            ),
            (
                "[review]\nlane_timeout_secs = 600\n",
                "[review].lane_timeout_secs",
            ),
            ("[review]\nlane_max_turns = 40\n", "[review].lane_max_turns"),
            (
                "[review.fact_check]\nmodel = \"m\"\n",
                "[review.fact_check]",
            ),
        ] {
            let text = format!("{MINIMAL}\n{model}{section}");
            let error = Config::parse(&text)
                .and_then(Config::into_settings)
                .unwrap_err();
            assert!(matches!(error, ConfigError::Review(_)), "{key}: {error}");
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("{key} was removed")),
                "{message}"
            );
            assert!(message.contains("[review.loop]"), "{message}");
            assert!(message.contains("Migrating from lanes"), "{message}");
            assert!(henk_domain::text::is_in_style(&message), "{message}");
        }
    }

    #[test]
    fn the_repeat_guard_limit_reaches_the_settings() {
        let settings = Config::parse(MINIMAL)
            .and_then(Config::into_settings)
            .unwrap();
        assert_eq!(settings.agent.max_repeated_calls, 3, "the default");
        assert!(
            settings
                .describe()
                .contains("3 identical tool calls in a row")
        );

        let text = format!("{MINIMAL}\n[agent]\nmax_repeated_calls = 5\n");
        let settings = Config::parse(&text)
            .and_then(Config::into_settings)
            .unwrap();
        assert_eq!(settings.agent.max_repeated_calls, 5);

        let text = format!("{MINIMAL}\n[agent]\nmax_repeated_calls = 0\n");
        let settings = Config::parse(&text)
            .and_then(Config::into_settings)
            .unwrap();
        assert_eq!(settings.agent.max_repeated_calls, 0);
        assert!(settings.describe().contains("Agent guard:     off"));

        let text = format!("{MINIMAL}\n[agent]\nmax_repeat = 5\n");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Syntax(_))));
        let text = format!("{MINIMAL}\n[agent]\nmax_repeated_calls = -1\n");
        assert!(matches!(Config::parse(&text), Err(ConfigError::Syntax(_))));
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
    fn planning_requester_must_be_a_team_lead() {
        let text = format!(
            "{MINIMAL}\n[models.m]\nprovider = \"anthropic\"\nbase_url = \"https://x\"\napi_key_env = \"K\"\nmodel = \"c\"\n[planning]\nrequester_id = 99\n[roles.planner]\nmodel = \"m\"\n[roles.reviewer]\nmodel = \"m\"\n"
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
    fn review_ignore_defaults_to_lockfiles_and_the_changelog() {
        let settings = Config::parse(MINIMAL)
            .and_then(Config::into_settings)
            .unwrap();
        assert!(settings.review.ignore.contains(&"**/Cargo.lock".to_owned()));
        assert!(settings.review.ignore.contains(&"CHANGELOG.md".to_owned()));
        let off = format!("{MINIMAL}\n[review]\nignore = []\n");
        let settings = Config::parse(&off).and_then(Config::into_settings).unwrap();
        assert!(settings.review.ignore.is_empty());
        let empty = format!("{MINIMAL}\n[review]\nignore = [\"*.lock\", \" \"]\n");
        assert!(matches!(
            Config::parse(&empty).and_then(Config::into_settings),
            Err(ConfigError::Review(_))
        ));
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
