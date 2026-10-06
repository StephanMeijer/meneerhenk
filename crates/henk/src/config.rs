//! Configuration: identities, the allowlist, models, lanes and MCP servers.
//!
//! No credential lives here (§8.4). Every secret is named by the environment
//! variable that holds it and read by the component that needs it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use serde::Deserialize;

use henk_domain::allowlist::{AllowRule, Allowlist, Platform, RepoRef};
use henk_domain::commit::{CommitPerson, Email, IdentityError, TrailerPolicy};
use henk_domain::discord::HenkIdentity;
use henk_domain::identity::{DiscordChannelId, DiscordRoleId, DiscordUserId, People, Person};
use henk_domain::mail::EmailAddress;
use henk_domain::marker::ModelId;
use henk_domain::review::{LaneName, LaneSpec};
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
    /// Planning settings.
    #[serde(default)]
    pub planning: Option<PlanningConfig>,
    /// Addressing review feedback (§3.5).
    #[serde(default)]
    pub address: Option<AddressConfig>,
    /// Where an address run's tools and checks run (§3.5).
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    /// External MCP servers by alias.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
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
    /// Inbound events and their outcomes older than this many days are
    /// deleted while serving. Runs are kept: their links are posted.
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
    /// it, and address runs (§3.5) push and read the merge request with it.
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
    /// Changed files no lane reviews (§3.2): lockfiles and generated
    /// changelogs by default. A pattern without `/` matches the file name at
    /// any depth; `**` matches any number of directories. Setting the key
    /// replaces the default; `ignore = []` reviews everything.
    #[serde(default = "default_review_ignore")]
    pub ignore: Vec<String>,
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
    /// The skills the checker may load.
    #[serde(default)]
    pub skills: Vec<SkillName>,
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
            ignore: default_review_ignore(),
        }
    }
}

/// Limits every model session shares: review lanes, fact-checks, planners
/// and address runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentFileConfig {
    /// Identical tool calls in a row a session may make. The next one is
    /// refused, and one more in a later turn ends the session as stuck. 0
    /// turns the guard off.
    pub max_repeated_calls: u32,
}

impl Default for AgentFileConfig {
    fn default() -> Self {
        Self {
            max_repeated_calls: henk_domain::repeat::DEFAULT_LIMIT,
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
    /// The skills this lane may load.
    #[serde(default)]
    pub skills: Vec<SkillName>,
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
    /// The skills the planner may load.
    #[serde(default)]
    pub skills: Vec<SkillName>,
}

/// Addressing review feedback (§3.5).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressConfig {
    /// Model id from `[models]`.
    pub model: String,
    /// Time limit in seconds.
    #[serde(default = "default_address_timeout_secs")]
    pub timeout_secs: u64,
    /// Model calls.
    #[serde(default = "default_plan_max_turns")]
    pub max_turns: u32,
    /// Files one run may change.
    #[serde(default = "default_max_changed_files")]
    pub max_changed_files: usize,
    /// The project's checks, each an argument list such as
    /// `["cargo", "test"]`, run in the workspace with an empty environment.
    #[serde(default)]
    pub check_commands: Vec<Vec<String>>,
    /// Deprecated: the time limit per check in seconds. Use
    /// `[workspace] command_secs`, which this overrides when set.
    #[serde(default)]
    pub check_timeout_secs: Option<u64>,
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

/// Which trailers an address-run commit carries. A switch left out keeps
/// the default, or for a repository the `[address.trailers]` value.
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

impl AddressConfig {
    /// The trailers for a commit to `repo`: its override, over
    /// `[address.trailers]`, over the defaults.
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

fn default_address_timeout_secs() -> u64 {
    30 * 60
}
fn default_max_changed_files() -> usize {
    20
}

/// Where an address run's tools and checks run (§3.5): the default
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
    /// Named profiles; what they leave out comes from the default.
    pub profiles: BTreeMap<String, ProfileConfig>,
    /// Repository, as `owner/name`, to profile name.
    pub repositories: BTreeMap<String, String>,
    /// The sandbox host the `ssh` backend uses (#84).
    pub ssh: Option<SshConfig>,
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
    /// The user whose key may run `henk-runner` there.
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
    "henk".to_owned()
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
            profiles: BTreeMap::new(),
            repositories: BTreeMap::new(),
            ssh: None,
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
}

impl WorkspaceConfig {
    /// The policy, with `check_timeout_secs` from `[address]` as the default
    /// profile's command limit when it is set.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Workspace`] when the policy is unusable.
    /// The policy and the sandbox host, checked together: a profile on the
    /// ssh backend needs `[workspace.ssh]`.
    fn into_parts(
        mut self,
        check_timeout_secs: Option<u64>,
    ) -> Result<(WorkspacePolicy, Option<SshConfig>), ConfigError> {
        let ssh = self.ssh.take();
        let policy = self.into_policy(check_timeout_secs)?;
        SshConfig::check(ssh.as_ref(), &policy)?;
        Ok((policy, ssh))
    }

    fn into_policy(self, check_timeout_secs: Option<u64>) -> Result<WorkspacePolicy, ConfigError> {
        let default = Profile {
            backend: self.backend,
            image: self.image,
            limits: Limits {
                command_secs: check_timeout_secs.unwrap_or(self.command_secs),
                run_secs: self.run_secs,
                output_bytes: self.output_bytes,
                memory_mib: self.memory_mib,
                cpus: self.cpus,
                pids: self.pids,
                disk_mib: self.disk_mib,
            },
            setup: self.setup,
            toolchain: self.toolchain,
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
    /// Their GitHub user id: the address-run commit credits them with the
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
    /// A review setting is unusable.
    #[error("{0}")]
    Review(String),
    /// The address setting is unusable.
    #[error("{0}")]
    Address(String),
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
    /// Review lanes.
    pub lanes: Vec<LaneSpec>,
    /// Lane limits.
    pub review: ReviewConfig,
    /// Planning.
    pub planning: Option<PlanningConfig>,
    /// Addressing review feedback.
    pub address: Option<AddressConfig>,
    /// Where an address run's tools and checks run, per repository.
    pub workspace: WorkspacePolicy,
    /// The sandbox host of the `ssh` backend, when configured.
    pub workspace_ssh: Option<SshConfig>,
    /// True when the command limit came from the deprecated
    /// `address.check_timeout_secs`.
    pub legacy_check_timeout: bool,
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
        let lanes = validate_lanes(&self.review.lanes, &self.models)?;
        if self.review.ignore.iter().any(|p| p.trim().is_empty()) {
            return Err(ConfigError::Review(
                "review.ignore has an empty pattern".to_owned(),
            ));
        }
        validate_fact_check(self.review.fact_check.as_ref(), &self.models)?;
        if let Some(planning) = &self.planning {
            validate_planning(planning, &self.models, &self.discord.team_lead_ids)?;
        }
        validate_mcp_references(self.github.as_ref(), self.gitlab.as_ref(), &self.mcp)?;
        if let Some(address) = &self.address {
            validate_address(
                address,
                &self.models,
                &self.discord.team_lead_ids,
                &allowlist,
            )?;
        }
        let skills_dir = self.skills.map(|s| s.dir);
        validate_skill_references(skills_dir.as_deref(), &self.review, self.planning.as_ref())?;
        let committers = self
            .people
            .iter()
            .map(|person| Ok((person.discord_id, person.committer()?)))
            .collect::<Result<BTreeMap<_, _>, ConfigError>>()?;
        let legacy_database_path = self.server.database_path.is_some();
        let check_timeout_secs = self.address.as_ref().and_then(|a| a.check_timeout_secs);
        let (workspace, workspace_ssh) = self.workspace.into_parts(check_timeout_secs)?;
        let database = resolve_database(self.database, self.server.database_path.clone())?;
        if let Some(dashboard) = &self.dashboard {
            validate_dashboard(dashboard)?;
        }
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
            address: self.address,
            workspace,
            workspace_ssh,
            legacy_check_timeout: check_timeout_secs.is_some(),
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

fn validate_address(
    address: &AddressConfig,
    models: &BTreeMap<String, ModelFileConfig>,
    team_leads: &[DiscordUserId],
    allowlist: &Allowlist,
) -> Result<(), ConfigError> {
    if !models.contains_key(&address.model) {
        return Err(ConfigError::UnknownModel {
            what: "address".to_owned(),
            model: address.model.clone(),
        });
    }
    if !team_leads.contains(&address.requester_id) {
        return Err(ConfigError::Address(format!(
            "address.requester_id {} is not in discord.team_lead_ids",
            address.requester_id
        )));
    }
    if address.max_changed_files == 0
        || address.timeout_secs == 0
        || address.max_turns == 0
        || address.check_timeout_secs == Some(0)
    {
        return Err(ConfigError::Address(
            "address limits must be above zero".to_owned(),
        ));
    }
    if address
        .check_commands
        .iter()
        .any(|argv| argv.first().is_none_or(|program| program.trim().is_empty()))
    {
        return Err(ConfigError::Address(
            "address.check_commands has an empty command".to_owned(),
        ));
    }
    address
        .henk_identity()
        .map_err(|e| ConfigError::Address(format!("address.identity: {e}")))?;
    for key in address.repositories.keys() {
        let allowed = [Platform::GitHub, Platform::GitLab]
            .into_iter()
            .any(|platform| {
                RepoRef::parse(platform, key).is_ok_and(|repo| allowlist.allows(&repo))
            });
        if !allowed {
            return Err(ConfigError::Address(format!(
                "address.repositories.{key:?} is not an owner/name repository on the allowlist"
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

/// The planner's model exists and its requester is a Team Lead.
fn validate_planning(
    planning: &PlanningConfig,
    models: &BTreeMap<String, ModelFileConfig>,
    team_lead_ids: &[DiscordUserId],
) -> Result<(), ConfigError> {
    if !models.contains_key(&planning.model) {
        return Err(ConfigError::UnknownModel {
            what: "planning".to_owned(),
            model: planning.model.clone(),
        });
    }
    if !team_lead_ids.contains(&planning.requester_id) {
        return Err(ConfigError::PlanningRequester(planning.requester_id));
    }
    Ok(())
}

fn validate_fact_check(
    fact_check: Option<&FactCheckConfig>,
    models: &BTreeMap<String, ModelFileConfig>,
) -> Result<(), ConfigError> {
    let Some(fact_check) = fact_check else {
        return Ok(());
    };
    let named = std::iter::once(&fact_check.model).chain(&fact_check.backup_model);
    for model in named {
        if !models.contains_key(model) {
            return Err(ConfigError::UnknownModel {
                what: "review.fact_check".to_owned(),
                model: model.clone(),
            });
        }
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
fn validate_skill_references(
    dir: Option<&str>,
    review: &ReviewConfig,
    planning: Option<&PlanningConfig>,
) -> Result<(), ConfigError> {
    if dir.is_some_and(|d| d.trim().is_empty()) {
        return Err(ConfigError::Skill("skills.dir is empty".to_owned()));
    }
    if dir.is_some() {
        return Ok(());
    }
    let lists = review
        .lanes
        .iter()
        .filter(|lane| !lane.skills.is_empty())
        .map(|lane| format!("lane {}", lane.name))
        .chain(
            review
                .fact_check
                .as_ref()
                .filter(|f| !f.skills.is_empty())
                .map(|_| "review.fact_check".to_owned()),
        )
        .chain(
            planning
                .filter(|p| !p.skills.is_empty())
                .map(|_| "planning".to_owned()),
        )
        .next();
    match lists {
        Some(what) => Err(ConfigError::Skill(format!(
            "{what} lists skills, but there is no [skills] section to load them from"
        ))),
        None => Ok(()),
    }
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
            skills: lane.skills.clone(),
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
            if name == "default" && self.legacy_check_timeout {
                out.push_str(
                    " (address.check_timeout_secs is deprecated; use [workspace] command_secs)",
                );
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
            };
        }
    }

    /// The lanes, the fact-check, planning and their skills.
    fn describe_agents(&self, out: &mut String) {
        let _ = writeln!(out, "Review lanes:");
        for lane in &self.lanes {
            let _ = writeln!(
                out,
                "  {} on model {}{}",
                lane.name,
                lane.model,
                skills_note(&lane.skills)
            );
        }
        if let Some(fact_check) = &self.review.fact_check {
            let _ = writeln!(
                out,
                "Fact-check:      model {}{}{}",
                fact_check.model,
                fact_check
                    .backup_model
                    .as_deref()
                    .map_or(String::new(), |b| format!(", backup {b}")),
                skills_note(&fact_check.skills)
            );
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
            if !planning.skills.is_empty() {
                let _ = writeln!(
                    out,
                    "                 {}",
                    skills_note(&planning.skills).trim_start_matches("; ")
                );
            }
        }
        if let Some(address) = &self.address {
            let _ = writeln!(
                out,
                "Address runs:    model {} for Team Lead {}; {}s, {} turns, {} files, {} checks",
                address.model,
                address.requester_id,
                address.timeout_secs,
                address.max_turns,
                address.max_changed_files,
                address.check_commands.len()
            );
            self.describe_commits(address, out);
        }
        self.describe_skills(out);
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
    fn describe_commits(&self, address: &AddressConfig, out: &mut String) {
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
            "Review limits:   {} at once, {}s and {} turns per lane",
            self.review.max_concurrent, self.review.lane_timeout_secs, self.review.lane_max_turns
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
        assert_eq!(settings.lanes.len(), 2);
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
    fn address_runs_are_configured_and_checked() {
        let with_model = format!(
            "{MINIMAL}[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\nmodel = \"x\"\n"
        );
        let parse = |extra: &str| {
            Config::parse(&format!("{with_model}{extra}")).and_then(Config::into_settings)
        };
        let settings = parse(
            "[address]\nmodel = \"m\"\nrequester_id = 3\ncheck_commands = [[\"cargo\", \"test\"]]\n",
        )
        .unwrap();
        let address = settings.address.as_ref().unwrap();
        assert_eq!(address.timeout_secs, 1800);
        assert_eq!(address.max_changed_files, 20);
        assert_eq!(address.check_commands, [["cargo", "test"]]);
        assert!(settings.describe().contains("Address runs:    model m"));

        for (bad, why) in [
            (
                "[address]\nmodel = \"nope\"\nrequester_id = 3\n",
                "unknown model",
            ),
            (
                "[address]\nmodel = \"m\"\nrequester_id = 99\n",
                "not a Team Lead",
            ),
            (
                "[address]\nmodel = \"m\"\nrequester_id = 3\nmax_changed_files = 0\n",
                "zero limit",
            ),
            (
                "[address]\nmodel = \"m\"\nrequester_id = 3\ncheck_commands = [[]]\n",
                "empty command",
            ),
            (
                "[address]\nmodel = \"m\"\nrequester_id = 3\npush_force = true\n",
                "unknown field",
            ),
        ] {
            assert!(parse(bad).is_err(), "{why}");
        }
    }

    /// The example with its commented `[address]` settings and the commented
    /// fields of `[[people]]` switched on.
    fn example_with_address() -> String {
        let mut in_address = false;
        EXAMPLE
            .lines()
            .map(|line| {
                in_address |= line == "# [address]";
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
        let address = settings.address.as_ref().unwrap();
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

    const ADDRESS: &str = "[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\nmodel = \"x\"\n[[people]]\ndiscord_id = 3\nname = \"Lead\"\nrole = \"team lead\"\nPERSON[address]\nmodel = \"m\"\nrequester_id = 3\n";

    fn address(person: &str, extra: &str) -> Result<Settings, ConfigError> {
        let text = format!("{MINIMAL}{}{extra}", ADDRESS.replace("PERSON", person));
        Config::parse(&text).and_then(Config::into_settings)
    }

    #[test]
    fn trailers_default_and_resolve_per_repository() {
        let settings = address(
            "github_id = 77\n",
            "[address.trailers]\nhenk_signoff = false\n[address.repositories.\"docspec/dco\"]\nrequester_signoff = true\n",
        )
        .unwrap();
        let config = settings.address.as_ref().unwrap();
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
            "the repository's switch over [address.trailers], case aside"
        );
        let described = settings.describe();
        assert!(
            described.contains("Commits as:      the GitHub App's noreply address"),
            "{described}"
        );

        let plain = address("", "").unwrap();
        assert_eq!(
            plain.address.as_ref().unwrap().trailer_policy(&other),
            TrailerPolicy::default()
        );
    }

    #[test]
    fn a_requester_without_an_account_is_warned_about() {
        let warning = "Warning:         requester 3 has no github_id";
        let described = address("", "").unwrap().describe();
        assert!(described.contains(warning), "{described}");
        let off = address("", "[address.trailers]\nrequester_coauthor = false\n")
            .unwrap()
            .describe();
        assert!(!off.contains(warning), "nothing to credit: {off}");
        let on_somewhere = address(
            "",
            "[address.trailers]\nrequester_coauthor = false\n[address.repositories.\"docspec/r\"]\nrequester_signoff = true\n",
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
                "[address.identity]\nname = \"Henk\"\nemail = \"not-an-address\"\n",
                "bad identity email",
            ),
            (
                "",
                "[address.identity]\nname = \"Henk <h@x.nl>\"\nemail = \"h@x.nl\"\n",
                "bad identity name",
            ),
            (
                "",
                "[address.identity]\nname = \"Henk\"\n",
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
                "[address.repositories.\"elsewhere/r\"]\nrequester_signoff = true\n",
                "repository not on the allowlist",
            ),
            (
                "",
                "[address.repositories.\"docspec\"]\nrequester_signoff = true\n",
                "not owner/name",
            ),
            (
                "",
                "[address.repositories.\"docspec/r\"]\nrequester_sign_off = true\n",
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
        assert!(!defaults.legacy_check_timeout);

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

        let with_model = format!(
            "{MINIMAL}[models.m]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\nmodel = \"x\"\n"
        );
        let legacy = Config::parse(&format!(
            "{with_model}[address]\nmodel = \"m\"\nrequester_id = 3\ncheck_timeout_secs = 42\n"
        ))
        .and_then(Config::into_settings)
        .unwrap();
        assert!(legacy.legacy_check_timeout);
        assert_eq!(legacy.workspace.default.limits.command_secs, 42);
        assert!(
            legacy
                .describe()
                .contains("address.check_timeout_secs is deprecated"),
            "{}",
            legacy.describe()
        );
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
            (22, "henk", "HENK_SANDBOX_KEY_PATH")
        );
        let text = settings.describe();
        assert!(text.contains("  sandbox: ssh, 600s per command"), "{text}");
        assert!(
            text.contains(
                "  sandbox host: henk@sandbox.example:22 (key from $HENK_SANDBOX_KEY_PATH)"
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
        assert!(settings.lanes.is_empty());
        assert_eq!(settings.server.bind, "127.0.0.1:8080");
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
