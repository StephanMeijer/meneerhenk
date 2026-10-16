//! `henk doctor`: checks a deployment before the first webhook arrives.
//!
//! Every check is independent and reported on its own line, so one missing
//! secret does not hide the next problem.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use henk_domain::workspace::{BackendKind, Toolchain, WorkspacePolicy};
use henk_llm::chatgpt::{Credentials, LoadError};
use henk_mcp::{McpSession as _, RmcpSession};
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth, Installations};

use crate::app::env_var;
use crate::config::{DEFAULT_SSH_HOST, DatabaseConfig, Settings, SshHost};

/// The outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Fine.
    Ok(String),
    /// Works, but worth knowing.
    Warn(String),
    /// Will not work.
    Fail(String),
}

/// One check and its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// How it went.
    pub verdict: Verdict,
}

impl Check {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            verdict: Verdict::Ok(detail.into()),
        }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            verdict: Verdict::Warn(detail.into()),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            verdict: Verdict::Fail(detail.into()),
        }
    }

    /// Whether this check blocks running Henk.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self.verdict, Verdict::Fail(_))
    }
}

/// Runs every check. `probe_models` sends one short prompt to each model.
pub async fn run(settings: &Settings, probe_models: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.extend(check_secrets(settings));
    checks.extend(check_models(settings, probe_models).await);
    let (github, installation) = check_github(settings, probe_models).await;
    checks.extend(github);
    checks.extend(check_mcp(settings, installation).await);
    checks.extend(check_mcp_server(settings, probe_models, env_var).await);
    checks.push(check_database(settings).await);
    checks.extend(check_sandbox(settings, probe_models).await);
    checks.extend(check_kubernetes(settings, probe_models).await);
    checks.push(check_git(settings, None).await);
    checks
}

/// Henk's own MCP server (#250): each client token's variable, set or not
/// (never its value), and with `--probe` an `initialize` and `tools/list`
/// against the running server at the public base URL, with the first token
/// that is set, as a client would. `lookup` reads a variable: `env_var`,
/// or a test's.
pub async fn check_mcp_server(
    settings: &Settings,
    probe: bool,
    lookup: impl Fn(&str) -> Option<String> + Clone,
) -> Vec<Check> {
    const NAME: &str = "Henk's MCP server";
    let Some(config) = &settings.mcp_server else {
        return Vec::new();
    };
    if !config.enabled {
        return vec![Check::ok(NAME, "off; /mcp answers 503")];
    }
    let mut checks: Vec<Check> = config
        .tokens
        .iter()
        .map(|token| {
            let name = format!("secret ${}", token.env);
            if lookup(&token.env).is_some() {
                Check::ok(
                    name,
                    format!("set; MCP client {} ({})", token.name, token.scope.as_str()),
                )
            } else {
                Check::warn(
                    name,
                    format!("not set; MCP client {} cannot connect", token.name),
                )
            }
        })
        .collect();
    let Some(usable) = config.tokens.iter().find(|t| lookup(&t.env).is_some()) else {
        checks.push(Check::fail(
            NAME,
            "on, but no token's variable is set: /mcp answers 503",
        ));
        return checks;
    };
    let url = format!(
        "{}/mcp",
        settings.server.public_base_url.trim_end_matches('/')
    );
    if !probe {
        checks.push(Check::ok(NAME, format!("on at {url}; --probe connects")));
        return checks;
    }
    let client = henk_mcp::McpServerConfig {
        command: None,
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        pass_env: Vec::new(),
        url: Some(url.clone()),
        bearer_env: Some(usable.env.clone()),
        call_timeout_secs: 30,
    };
    let probed = tokio::time::timeout(
        Duration::from_secs(30),
        RmcpSession::connect("henk", &client, lookup),
    )
    .await;
    checks.push(match probed {
        Ok(Ok(session)) => {
            let listed = session.list_tools().await;
            session.close().await;
            match listed {
                Ok(tools) => Check::ok(
                    NAME,
                    format!("{} tools at {url}, as {}", tools.len(), usable.name),
                ),
                Err(error) => Check::fail(NAME, format!("{url}: tools/list failed: {error}")),
            }
        }
        Ok(Err(error)) => Check::fail(NAME, format!("{url}: {error}")),
        Err(_) => Check::fail(NAME, format!("{url} did not answer within 30s")),
    });
    checks
}

/// Whether `git` runs in Henk's own process, where reviews and the planner
/// check out a repository and the executor's fixes are pushed (#254). Local and quick, so it
/// runs without `--probe` too. `path` replaces `PATH`, for a test.
pub async fn check_git(settings: &Settings, path: Option<&std::ffi::OsStr>) -> Check {
    let needs = settings.needs_git();
    if needs.is_empty() {
        return Check::ok(
            "git",
            "not needed: no profile checks out a repository and nothing pushes",
        );
    }
    let why = needs.join(", ");
    match crate::git::version(path).await {
        Ok(version) => Check::ok("git", format!("{version}; needed for {why}")),
        Err(error) => Check::fail(
            "git",
            format!(
                "{error}: needed for {why}. Henk runs git itself to check out the repository; install git in Henk's image"
            ),
        ),
    }
}

/// The `ssh` backend's sandbox host (#84): without `--probe`, only whether
/// its key is set; with it, the connection, the host key and the tools the
/// sandbox script needs there, each on a line of its own.
async fn check_sandbox(settings: &Settings, probe: bool) -> Vec<Check> {
    let Some(ssh) = &settings.workspace_ssh else {
        return Vec::new();
    };
    let hosts = ssh.hosts();
    // One host keeps the names it always had; several name each (#389).
    let named = !(hosts.len() == 1 && hosts.iter().all(|h| h.id == DEFAULT_SSH_HOST));
    let mut checks: Vec<Check> = Vec::new();
    let mut usable = Vec::new();
    for host in &hosts {
        let (host_checks, works) = check_sandbox_host(settings, host, named, probe).await;
        // Hosts that share a key share its check.
        for check in host_checks {
            if !checks.contains(&check) {
                checks.push(check);
            }
        }
        if works {
            usable.push(host.id.as_str());
        }
    }
    if probe && named && !usable.is_empty() && usable.len() < hosts.len() {
        let down: Vec<&str> = hosts
            .iter()
            .map(|h| h.id.as_str())
            .filter(|id| !usable.contains(id))
            .collect();
        checks.push(Check::warn(
            "sandbox hosts",
            format!(
                "{} of {} usable; down: {}",
                usable.len(),
                hosts.len(),
                down.join(", ")
            ),
        ));
    }
    checks
}

/// One sandbox host's checks, and whether it works.
async fn check_sandbox_host(
    settings: &Settings,
    host: &SshHost,
    named: bool,
    probe: bool,
) -> (Vec<Check>, bool) {
    let label = |what: &str| {
        if named {
            format!("sandbox {} {what}", host.id)
        } else {
            format!("sandbox {what}")
        }
    };
    let target = format!("{}@{}:{}", host.user, host.host, host.port);
    let key = format!("secret ${}", host.key_path_env);
    if env_var(&host.key_path_env).is_none() {
        return (
            vec![Check::fail(
                key,
                "not set; the ssh workspace backend cannot sign in",
            )],
            false,
        );
    }
    if !probe {
        return (
            vec![
                Check::ok(key, "set; Henk's key for the sandbox host"),
                Check::ok(label("host"), format!("{target}; --probe connects")),
            ],
            true,
        );
    }
    let provider = match crate::workspace::ssh_target(host) {
        Ok(target) => crate::workspace::ssh::SshProvider::new(target),
        Err(error) => return (vec![Check::fail(key, format!("{error:#}"))], false),
    };
    match provider.probe().await {
        Ok(report) => (
            vec![
                Check::ok(label("connection"), format!("{target}, signed in")),
                Check::ok(label("host key"), "matches the pinned host_key"),
                tools_check(
                    &label("host tools"),
                    &report,
                    &settings.workspace,
                    BackendKind::Ssh,
                    Some(&host.id),
                ),
            ],
            true,
        ),
        Err(error) => {
            let text = error.to_string();
            let refused_key = text.contains("not the pinned");
            (
                vec![
                    Check::fail(label("connection"), format!("{target}: {text}")),
                    if refused_key {
                        Check::fail(label("host key"), "not the pinned host_key")
                    } else {
                        Check::warn(label("host key"), "not checked: no connection")
                    },
                ],
                false,
            )
        }
    }
}

/// The sandbox script's probe report as a check named `name`: a tool it
/// says is missing fails it, mise only when a profile on `backend` names it
/// as its toolchain, and on a sandbox host `host` only when that profile
/// may use the host; a host-backed profile runs mise on Henk's own machine.
fn tools_check(
    name: &str,
    report: &str,
    workspace: &WorkspacePolicy,
    backend: BackendKind,
    host: Option<&str>,
) -> Check {
    let mut lines = report.lines();
    let version = lines.next().unwrap_or("henk-sandbox (no version)");
    let wants_mise = workspace.named().any(|(_, p)| {
        p.backend == backend
            && p.toolchain == Some(Toolchain::Mise)
            && host.is_none_or(|id| p.hosts.is_empty() || p.hosts.iter().any(|h| h == id))
    });
    let missing: Vec<&str> = lines
        .filter_map(|l| l.strip_suffix(" missing"))
        .filter(|tool| *tool != "mise" || wants_mise)
        .collect();
    if missing.is_empty() {
        Check::ok(name, version)
    } else {
        Check::fail(
            name,
            format!("{version}; missing there: {}", missing.join(", ")),
        )
    }
}

/// The `kubernetes` backend's cluster (#89): without `--probe`, only the
/// namespace; with it, what Henk may do there, as the API server says, and
/// the tools of every image a profile starts its Pods from, each on a line
/// of its own.
async fn check_kubernetes(settings: &Settings, probe: bool) -> Vec<Check> {
    let Some(config) = &settings.workspace_kubernetes else {
        return Vec::new();
    };
    let namespace = &config.namespace;
    if !probe {
        return vec![Check::ok(
            "sandbox cluster",
            format!("namespace {namespace}; --probe asks the API server"),
        )];
    }
    let provider = match crate::workspace::kubernetes_provider(settings).await {
        Ok(Some(provider)) => provider,
        Ok(None) => return Vec::new(),
        Err(error) => return vec![Check::fail("sandbox cluster", format!("{error:#}"))],
    };
    let access = match provider.access().await {
        Ok(access) => access,
        Err(error) => return vec![Check::fail("sandbox cluster", error.to_string())],
    };
    let mut checks = vec![Check::ok(
        "sandbox cluster",
        format!("reached; Pods go to namespace {namespace}"),
    )];
    checks.push(access_check(namespace, &access));
    let mut images: Vec<&str> = settings
        .workspace
        .named()
        .filter(|(_, p)| p.backend == BackendKind::Kubernetes)
        .filter_map(|(_, p)| config.image_for(p))
        .collect();
    images.sort_unstable();
    images.dedup();
    for image in images {
        let name = format!("sandbox image {image}");
        checks.push(match provider.probe(image).await {
            Ok(report) => tools_check(
                &name,
                &report,
                &settings.workspace,
                BackendKind::Kubernetes,
                None,
            ),
            Err(error) => Check::fail(name, error.to_string()),
        });
    }
    checks
}

/// What Henk may do in the sandbox namespace as a check: everything the
/// backend needs, and no Secrets.
fn access_check(namespace: &str, access: &[(String, bool)]) -> Check {
    let missing: Vec<&str> = access
        .iter()
        .filter(|(what, allowed)| !allowed && what != "get secrets")
        .map(|(what, _)| what.as_str())
        .collect();
    let secrets = access
        .iter()
        .any(|(what, allowed)| *allowed && what == "get secrets");
    if !missing.is_empty() {
        Check::fail(
            "sandbox access",
            format!(
                "Henk may not {} in namespace {namespace}; apply deploy/kubernetes/sandbox-namespace.yaml",
                missing.join(", ")
            ),
        )
    } else if secrets {
        Check::fail(
            "sandbox access",
            format!(
                "Henk may read Secrets in namespace {namespace}; give him Pods and pods/exec there and nothing more"
            ),
        )
    } else {
        Check::ok(
            "sandbox access",
            format!("Pods and pods/exec in namespace {namespace}, no Secrets"),
        )
    }
}

pub(crate) fn check_secrets(settings: &Settings) -> Vec<Check> {
    let server = &settings.server;
    let mut checks: Vec<Check> = [
        (
            &server.github_webhook_secret_env,
            "GitHub webhooks",
            settings.github.is_some(),
        ),
        (
            &server.gitlab_webhook_token_env,
            "GitLab webhooks",
            settings.gitlab.is_some(),
        ),
        (&server.api_token_env, "the review and plan API", true),
    ]
    .into_iter()
    .map(|(variable, purpose, needed)| {
        let name = format!("secret ${variable}");
        match (env_var(variable).is_some(), needed) {
            (true, _) => Check::ok(name, format!("set; used for {purpose}")),
            (false, true) => Check::warn(name, format!("not set; {purpose} will answer 503")),
            (false, false) => {
                Check::ok(name, format!("not set; {purpose} is not configured anyway"))
            }
        }
    })
    .collect();
    if let Some(gitlab) = &settings.gitlab {
        checks.push(gitlab_token(
            &gitlab.token_env,
            settings.roles.executor.is_some(),
        ));
    }
    if let Some(dashboard) = &settings.dashboard {
        let variable = &dashboard.client_id_env;
        checks.push(dashboard_client_id(variable, env_var(variable).as_deref()));
    }
    checks
}

/// Which kind of app the dashboard signs in with (#270). A GitHub App makes
/// GitHub ask people to let it act on their behalf; an OAuth App without
/// scopes asks only who they are, which is all the dashboard needs. Never
/// shows the id.
fn dashboard_client_id(variable: &str, value: Option<&str>) -> Check {
    let name = format!("secret ${variable}");
    match value {
        None => Check::warn(name, "not set; the dashboard is off"),
        Some(id) if crate::dashboard::is_github_app_client_id(id) => Check::warn(
            name,
            "a GitHub App's client id; GitHub asks people to let the dashboard act on \
             their behalf. Use an OAuth App without scopes",
        ),
        Some(_) => Check::ok(
            name,
            "an OAuth App's client id; sign-in asks only who you are",
        ),
    }
}

/// Henk reads the GitLab token himself only to push the executor's fixes,
/// and for the REST reads the MCP server does not give. Set or not set,
/// never its value.
fn gitlab_token(variable: &str, pushes: bool) -> Check {
    let name = format!("secret ${variable}");
    match (env_var(variable).is_some(), pushes) {
        (true, true) => Check::ok(name, "set; reviews on GitLab can push"),
        (true, false) => Check::ok(name, "set; there is no [roles.executor], so nothing pushes"),
        (false, true) => Check::warn(name, "not set; reviews on GitLab cannot push"),
        (false, false) => Check::ok(
            name,
            "not set; there is no [roles.executor], so nothing pushes",
        ),
    }
}

async fn check_models(settings: &Settings, probe: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    if settings.models.is_empty() {
        checks.push(Check::fail(
            "models",
            "none configured; reviews and plans cannot run",
        ));
        return checks;
    }
    // Each endpoint once (#368), then each model on a usable one.
    let mut usable = std::collections::BTreeSet::new();
    for id in settings.endpoints.keys() {
        let check = check_endpoint(settings, id, probe).await;
        if !check.is_failure() {
            usable.insert(id.as_str());
        }
        checks.push(check);
    }
    for (id, model) in &settings.models {
        checks.push(if usable.contains(model.endpoint.as_str()) {
            check_model(settings, id, probe).await
        } else {
            Check::fail(
                format!("model {id}"),
                format!("its endpoint {} is not usable (see above)", model.endpoint),
            )
        });
    }
    let unused = settings.unused_models();
    if !unused.is_empty() {
        checks.push(Check::warn(
            "models",
            format!("configured but used by no role: {}", unused.join(", ")),
        ));
    }
    let idle: Vec<&str> = settings
        .endpoints
        .keys()
        .filter(|id| settings.models.values().all(|m| &m.endpoint != *id))
        .map(String::as_str)
        .collect();
    if !idle.is_empty() {
        checks.push(Check::warn(
            "endpoints",
            format!("configured but used by no model: {}", idle.join(", ")),
        ));
    }
    if settings.review.r#loop.is_none() {
        checks.push(Check::fail(
            "reviews",
            "off: no [review.loop] is configured, so no pull request is reviewed",
        ));
    }
    checks
}

/// One endpoint: its key, and with `probe` the models it serves, which
/// should include those configured on it.
async fn check_endpoint(settings: &Settings, id: &str, probe: bool) -> Check {
    let name = format!("endpoint {id}");
    if let Some(file) = settings
        .endpoints
        .get(id)
        .filter(|endpoint| endpoint.signs_in_with_chatgpt())
        .and_then(|endpoint| endpoint.sign_in_file())
    {
        return check_sign_in(settings, id, file, probe).await;
    }
    let config = match settings.endpoint_config(id, env_var) {
        Ok(config) => config,
        Err(why) => return Check::fail(name, why),
    };
    if !probe {
        return Check::ok(
            name,
            format!(
                "{} at {} (key present; use --probe to call it)",
                config.provider.as_str(),
                config.base_url
            ),
        );
    }
    match tokio::time::timeout(Duration::from_mins(1), henk_llm::list_models(&config)).await {
        Ok(Ok(served)) => {
            let missing: Vec<&str> = settings
                .models
                .values()
                .filter(|m| m.endpoint == id && !served.contains(&m.model))
                .map(|m| m.model.as_str())
                .collect();
            if missing.is_empty() {
                Check::ok(name, format!("serves {} models", served.len()))
            } else {
                Check::warn(
                    name,
                    format!(
                        "serves {} models, but not {}",
                        served.len(),
                        missing.join(", ")
                    ),
                )
            }
        }
        // Not every endpoint lists its models; the model probes say whether
        // it answers.
        Ok(Err(error)) => Check::warn(name, format!("could not list its models: {error}")),
        Err(_) => Check::warn(name, "no model list within 60s"),
    }
}

/// An endpoint paid by a ChatGPT sign-in (#620): the sign-in file, and with
/// `probe` the models the plan lists.
///
/// The live call is made only while the access token has more than ten
/// minutes left. A renewal from here could meet the renewal of a running
/// Henk, and each renewal replaces the refresh token; the one that loses the
/// race would have a token OpenAI no longer takes.
async fn check_sign_in(settings: &Settings, id: &str, file: &Path, probe: bool) -> Check {
    let loaded = Credentials::load(file).map(|credentials| {
        let mode = std::fs::metadata(file).ok().map(|metadata| {
            std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o777
        });
        (credentials, mode)
    });
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
    )
    .unwrap_or(i64::MAX);
    let check = sign_in_check(id, &loaded, now);
    if check.is_failure() || !probe {
        return check;
    }
    let Ok((credentials, _)) = &loaded else {
        return check;
    };
    let name = format!("endpoint {id}");
    if credentials.expires_at.saturating_sub(now) <= PROBE_MIN_LEFT {
        return Check::warn(
            name,
            format!(
                "{}; no live call from here, because the access token is due for renewal and a running Henk may be renewing it. Run it again after Henk made a request, or right after signing in",
                detail_of(&check)
            ),
        );
    }
    let config = match settings.endpoint_config(id, env_var) {
        Ok(config) => config,
        Err(why) => return Check::fail(name, why),
    };
    match tokio::time::timeout(Duration::from_mins(1), henk_llm::list_models(&config)).await {
        Ok(Ok(served)) => {
            let missing: Vec<&str> = settings
                .models
                .values()
                .filter(|m| m.endpoint == id && !served.contains(&m.model))
                .map(|m| m.model.as_str())
                .collect();
            if missing.is_empty() {
                Check::ok(
                    name,
                    format!(
                        "{}; the plan lists {} models",
                        detail_of(&check),
                        served.len()
                    ),
                )
            } else {
                Check::warn(
                    name,
                    format!(
                        "{}; the plan lists {} models, but not {}",
                        detail_of(&check),
                        served.len(),
                        missing.join(", ")
                    ),
                )
            }
        }
        Ok(Err(error)) => Check::fail(name, format!("the plan could not be called: {error}")),
        Err(_) => Check::warn(name, "no model list within 60s"),
    }
}

/// Seconds the access token must have left for a live call from `doctor`.
const PROBE_MIN_LEFT: i64 = 600;

/// The text of a check's verdict.
fn detail_of(check: &Check) -> &str {
    match &check.verdict {
        Verdict::Ok(detail) | Verdict::Warn(detail) | Verdict::Fail(detail) => detail,
    }
}

/// What a ChatGPT sign-in file says, without a live call: whose it is, how
/// long its tokens last, and whether it needs attention. `loaded` is the
/// file and its permission bits; `now` is in seconds since the epoch.
fn sign_in_check(
    id: &str,
    loaded: &Result<(Credentials, Option<u32>), LoadError>,
    now: i64,
) -> Check {
    /// A refresh token lasts 30 days from the renewal that issued it.
    const REFRESH_DAYS: i64 = 30;
    const WARN_AFTER_DAYS: i64 = 25;
    let name = format!("endpoint {id}");
    let sign_in = format!("henk auth openai --endpoint {id}");
    let (credentials, mode) = match loaded {
        Ok(loaded) => loaded,
        Err(LoadError::Missing(file)) => {
            return Check::fail(
                name,
                format!(
                    "not signed in: {} does not exist; run `{sign_in}`",
                    file.display()
                ),
            );
        }
        Err(error @ LoadError::Unusable { .. }) => {
            return Check::fail(name, format!("{error}; run `{sign_in}`"));
        }
    };
    let account = credentials
        .account
        .as_deref()
        .unwrap_or("an unnamed account");
    if !credentials
        .scopes
        .iter()
        .any(|scope| scope == "chatgpt.tokens.use.direct")
    {
        return Check::fail(
            name,
            format!(
                "signed in as {account} without the scope that lets the plan pay; run `{sign_in}`"
            ),
        );
    }
    let left = credentials.expires_at.saturating_sub(now);
    let access = if left > 0 {
        format!("access token valid for {} min", left / 60)
    } else {
        "access token expired (renewed on first use)".to_owned()
    };
    let age_days = now.saturating_sub(credentials.refreshed_at) / 86_400;
    let detail = format!(
        "ChatGPT plan sign-in as {account}; {access}; renewed {age_days} days ago (a sign-in lapses {REFRESH_DAYS} days after its last renewal)"
    );
    if age_days >= REFRESH_DAYS {
        return Check::fail(
            name,
            format!("{detail}; it has most likely lapsed: run `{sign_in}`"),
        );
    }
    if age_days >= WARN_AFTER_DAYS {
        return Check::warn(
            name,
            format!("{detail}; it lapses soon unless a model call renews it, or run `{sign_in}`"),
        );
    }
    if let Some(mode) = mode.filter(|mode| mode & 0o077 != 0) {
        return Check::warn(
            name,
            format!("{detail}; the file is readable by others (mode {mode:o}): chmod 600 it"),
        );
    }
    Check::ok(name, detail)
}

/// One model on a usable endpoint; with `probe`, one short call.
/// The output cap of the probe's one prompt: 16 tokens for "ready", or
/// none, which leaves the model's configured cap. A thinking model spends
/// its cap on thinking first, and so does a reasoning model on the
/// Responses API (#365); cut short, it would stop before it answers.
fn probe_cap(settings: &Settings, model: &crate::config::ModelFileConfig) -> Option<u32> {
    let responses = settings
        .endpoints
        .get(&model.endpoint)
        .is_some_and(|endpoint| endpoint.api_in_use() == henk_llm::Api::Responses);
    (model.effort.is_none() && !responses).then_some(16)
}

async fn check_model(settings: &Settings, id: &str, probe: bool) -> Check {
    let name = format!("model {id}");
    let Some(model) = settings.models.get(id) else {
        return Check::fail(name, "not configured");
    };
    if !probe {
        return Check::ok(name, format!("{} on {}", model.model, model.endpoint));
    }
    let client = match settings
        .client_config(id, env_var)
        .and_then(|config| henk_llm::client_for(config).map_err(|e| e.to_string()))
    {
        Ok(client) => client,
        Err(why) => return Check::fail(name, why),
    };
    let request = henk_llm::CompletionRequest {
        messages: vec![henk_llm::ChatMessage::user(
            "Reply with the single word: ready",
        )],
        max_tokens: probe_cap(settings, model),
        ..Default::default()
    };
    match tokio::time::timeout(Duration::from_mins(1), client.complete(&request)).await {
        Ok(Ok(completion)) => Check::ok(
            name,
            format!("answered: {:?}", completion.message.text().trim()),
        ),
        Ok(Err(error)) => Check::fail(name, error.to_string()),
        Err(_) => Check::fail(name, "no answer within 60s"),
    }
}

/// One installation of the App, as `GET /app/installations` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Installation {
    id: u64,
    account: String,
}

/// The App's identity, then every installation: whether Henk may act for
/// it, whether its token mints, and with `probe` which repositories it
/// reaches. Allowlist entries no usable installation covers are warned
/// about. Also returns the App id and the first usable installation, which
/// the GitHub MCP server is started as.
async fn check_github(settings: &Settings, probe: bool) -> (Vec<Check>, Option<(u64, u64)>) {
    const NAME: &str = "GitHub App installation";
    let Some(github) = &settings.github else {
        return (vec![Check::ok("GitHub", "not configured")], None);
    };
    let (installations, api) = match github_client(github) {
        Ok(client) => client,
        Err(check) => return (vec![check], None),
    };
    let mut checks = vec![check_app_identity(&api, &github.bot_login).await];
    let listed = match list_installations(&api).await {
        Ok(listed) if listed.is_empty() => {
            checks.push(Check::fail(
                NAME,
                "the App is installed nowhere; install it on an account first",
            ));
            return (checks, None);
        }
        Ok(listed) => listed,
        Err(error) => {
            checks.push(Check::fail(NAME, error));
            return (checks, None);
        }
    };
    if let Some(ceiling) = github.ceiling() {
        for id in ceiling
            .iter()
            .filter(|id| !listed.iter().any(|i| i.id == **id))
        {
            checks.push(Check::fail(
                NAME,
                format!(
                    "installation {id} is configured but the App has no such installation; it has: {}",
                    describe_installations(&listed)
                ),
            ));
        }
    }
    let mut usable: Vec<(Installation, Option<Vec<String>>)> = Vec::new();
    for installation in &listed {
        let name = format!("{NAME} {} ({})", installation.id, installation.account);
        if !installations.allows(installation.id) {
            checks.push(Check::ok(
                name,
                "ignored: not one of [github] installations",
            ));
            continue;
        }
        let reached = reach(&api.for_installation(installation.id), probe).await;
        match reached {
            Ok((detail, repositories)) => {
                checks.push(Check::ok(name, detail));
                usable.push((installation.clone(), repositories));
            }
            Err(error) => checks.push(Check::fail(name, format!("token failed: {error}"))),
        }
    }
    checks.extend(uncovered(&settings.allowlist, &usable));
    let first = usable
        .first()
        .map(|(installation, _)| (github.app_id, installation.id));
    (checks, first)
}

/// The App client `github` configures, or the check that says why not.
fn github_client(
    github: &crate::config::GitHubConfig,
) -> Result<(Arc<Installations>, GitHubApi), Check> {
    let Some(path) = env_var(&github.private_key_path_env) else {
        return Err(Check::fail(
            "GitHub App key",
            format!(
                "environment variable {} is not set",
                github.private_key_path_env
            ),
        ));
    };
    let pem = std::fs::read_to_string(&path)
        .map_err(|error| Check::fail("GitHub App key", format!("cannot read {path}: {error}")))?;
    let credentials = AppCredentials::from_pem(github.app_id, &pem)
        .map_err(|error| Check::fail("GitHub App key", error.to_string()))?;
    let installations = Arc::new(Installations::new(credentials, github.ceiling()));
    let api = GitHubApi::new(
        &github.api_base,
        GitHubAuth::app(Arc::clone(&installations)),
    )
    .map_err(|error| Check::fail("GitHub client", error.to_string()))?;
    Ok((installations, api))
}

/// What an installation's token reaches: how many repositories, and with
/// `probe` which ones.
async fn reach(scoped: &GitHubApi, probe: bool) -> Result<(String, Option<Vec<String>>), String> {
    if probe {
        let repositories = tokio::time::timeout(Duration::from_mins(1), repositories(scoped))
            .await
            .unwrap_or_else(|_| Err("no answer within 60s".to_owned()))?;
        let detail = format!(
            "token works; {} repositories: {}",
            repositories.len(),
            repositories.join(", ")
        );
        return Ok((detail, Some(repositories)));
    }
    let value = tokio::time::timeout(
        Duration::from_secs(30),
        scoped.get("/installation/repositories?per_page=1"),
    )
    .await
    .map_err(|_| "no answer within 30s".to_owned())?
    .map_err(|error| error.to_string())?;
    let total = value
        .get("total_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Ok((format!("token works; {total} repositories visible"), None))
}

/// A warning for each GitHub allowlist entry no usable installation
/// reaches: its owner has none, or, when the repositories were listed, the
/// repository is not among them.
fn uncovered(
    allowlist: &henk_domain::allowlist::Allowlist,
    usable: &[(Installation, Option<Vec<String>>)],
) -> Vec<Check> {
    use henk_domain::allowlist::{AllowRule, Platform};
    let of = |owner: &str| -> Vec<&(Installation, Option<Vec<String>>)> {
        usable
            .iter()
            .filter(|(i, _)| i.account.eq_ignore_ascii_case(owner))
            .collect()
    };
    allowlist
        .rules()
        .iter()
        .filter_map(|rule| match rule {
            AllowRule::Owner {
                platform: Platform::GitHub,
                owner,
            } => of(owner).is_empty().then(|| {
                Check::warn(
                    "GitHub allowlist",
                    format!("github_owners {owner}: no usable installation of the App on {owner}; Henk cannot work there"),
                )
            }),
            AllowRule::Repository(repo) if repo.platform() == Platform::GitHub => {
                let path = repo.path();
                let installations = of(repo.owner());
                if installations.is_empty() {
                    return Some(Check::warn(
                        "GitHub allowlist",
                        format!("github_repositories {path}: no usable installation of the App on {}; Henk cannot work there", repo.owner()),
                    ));
                }
                let reached = installations.iter().any(|(_, repositories)| {
                    repositories
                        .as_ref()
                        .is_none_or(|list| list.iter().any(|r| r.eq_ignore_ascii_case(&path)))
                });
                (!reached).then(|| {
                    Check::warn(
                        "GitHub allowlist",
                        format!("github_repositories {path}: the App's installation on {} does not include it", repo.owner()),
                    )
                })
            }
            _ => None,
        })
        .collect()
}

/// Every repository an installation reaches, as `owner/name`.
async fn repositories(api: &GitHubApi) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for page in 1..=100 {
        let value = api
            .get(&format!(
                "/installation/repositories?per_page=100&page={page}"
            ))
            .await
            .map_err(|error| error.to_string())?;
        let listed = value
            .get("repositories")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        names.extend(listed.iter().filter_map(|r| {
            r.get("full_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }));
        if listed.len() < 100 {
            break;
        }
    }
    Ok(names)
}

/// `GET /app` as the App: proves the key and id match and that `bot_login`
/// is the App's slug.
async fn check_app_identity(api: &GitHubApi, bot_login: &str) -> Check {
    match tokio::time::timeout(Duration::from_secs(30), api.get_as_app("/app")).await {
        Ok(Ok(app)) => {
            let slug = app.get("slug").and_then(serde_json::Value::as_str);
            let name = app
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            match slug {
                Some(slug) if format!("{slug}[bot]") == bot_login => Check::ok(
                    "GitHub App",
                    format!("{name} ({slug}); key and app_id match; bot_login matches"),
                ),
                Some(slug) => Check::warn(
                    "GitHub App",
                    format!(
                        "{name} ({slug}); bot_login is {bot_login:?} but the App's login is \"{slug}[bot]\""
                    ),
                ),
                None => Check::warn("GitHub App", format!("{name}; no slug in the answer")),
            }
        }
        Ok(Err(error)) => Check::fail("GitHub App", format!("GET /app as the App failed: {error}")),
        Err(_) => Check::fail("GitHub App", "no answer within 30s"),
    }
}

/// The installations of the App.
async fn list_installations(api: &GitHubApi) -> Result<Vec<Installation>, String> {
    match tokio::time::timeout(
        Duration::from_secs(30),
        api.get_all_as_app("/app/installations"),
    )
    .await
    {
        Ok(Ok(installations)) => Ok(installations
            .iter()
            .filter_map(|i| {
                Some(Installation {
                    id: i.get("id").and_then(serde_json::Value::as_u64)?,
                    account: i
                        .pointer("/account/login")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?")
                        .to_owned(),
                })
            })
            .collect()),
        Ok(Err(error)) => Err(format!("could not list installations: {error}")),
        Err(_) => Err("could not list installations within 30s".to_owned()),
    }
}

fn describe_installations(listed: &[Installation]) -> String {
    listed
        .iter()
        .map(|i| format!("{} ({})", i.id, i.account))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Starts every MCP server and lists its tools. The GitHub one is started
/// as `github`'s installation, the first usable one, and skipped when there
/// is none.
async fn check_mcp(settings: &Settings, github: Option<(u64, u64)>) -> Vec<Check> {
    let mut checks = Vec::new();
    for (alias, config) in &settings.mcp {
        let name = format!("MCP server {alias}");
        let reads_github = settings
            .github
            .as_ref()
            .is_some_and(|g| &g.mcp_server == alias);
        let session_config;
        let config = match (reads_github, github) {
            (false, _) => config,
            (true, Some((app_id, installation))) => {
                session_config = crate::app::github_session_config(config, app_id, installation);
                &session_config
            }
            (true, None) => {
                checks.push(Check::warn(
                    name,
                    "not started: no usable GitHub App installation to sign it in as",
                ));
                continue;
            }
        };
        let missing: Vec<&str> = config
            .pass_env
            .iter()
            .filter(|v| env_var(v).is_none())
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            checks.push(Check::fail(
                name,
                format!("pass_env variables not set: {}", missing.join(", ")),
            ));
            continue;
        }
        match tokio::time::timeout(
            Duration::from_mins(2),
            RmcpSession::connect(alias, config, env_var),
        )
        .await
        {
            Ok(Ok(session)) => {
                match session.list_tools().await {
                    Ok(tools) => {
                        let platform = if alias.contains("gitlab") {
                            henk_domain::allowlist::Platform::GitLab
                        } else {
                            henk_domain::allowlist::Platform::GitHub
                        };
                        let exposed = tools
                            .iter()
                            .filter(|t| henk_domain::scope::is_exposed(platform, &t.name))
                            .count();
                        checks.push(Check::ok(
                            name,
                            format!("{} tools, {exposed} of them visible to lanes", tools.len()),
                        ));
                    }
                    Err(error) => checks.push(Check::fail(
                        name,
                        format!("started but tools/list failed: {error}"),
                    )),
                }
                session.close().await;
            }
            Ok(Err(error)) => checks.push(Check::fail(name, error.to_string())),
            Err(_) => checks.push(Check::fail(name, "did not start within 120s")),
        }
    }
    checks
}

async fn check_database(settings: &Settings) -> Check {
    let what = match &settings.database {
        DatabaseConfig::Sqlite { path } => path.clone(),
        DatabaseConfig::Postgres { url_env } => env_var(url_env)
            .and_then(|url| henk_store::describe_url(&url).ok())
            .map_or_else(
                || format!("${url_env}"),
                |place| format!("PostgreSQL at {place}"),
            ),
    };
    match crate::app::open_store(&settings.database).await {
        Ok(_) => Check::ok("database", format!("{what} opens and is migrated")),
        Err(error) => Check::fail("database", format!("{what}: {error:#}")),
    }
}

/// Renders checks as lines.
#[must_use]
pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let (tag, detail) = match &check.verdict {
            Verdict::Ok(d) => ("ok  ", d),
            Verdict::Warn(d) => ("warn", d),
            Verdict::Fail(d) => ("FAIL", d),
        };
        let _ = writeln!(out, "[{tag}] {}: {detail}", check.name);
    }
    let failures = checks.iter().filter(|c| c.is_failure()).count();
    let _ = write!(out, "{failures} failing check(s)");
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn settings(extra: &str) -> Settings {
        let text = format!(
            "[discord]\nchannel_id = 1\nhenk_user_id = 2\nteam_lead_ids = [3]\n[mail]\naddress = \"henk@example.com\"\n[allowlist]\ngithub_owners = [\"docspec\"]\n{extra}"
        );
        crate::config::Config::parse(&text)
            .and_then(crate::config::Config::into_settings)
            .unwrap()
    }

    const ADDRESS: &str = "[endpoints.m]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\n[models.m]\nendpoint = \"m\"\nmodel = \"x\"\n[roles.executor]\nmodel = \"m\"\nrequester_id = 3\n";

    #[tokio::test]
    async fn a_model_only_address_runs_use_is_not_unused() {
        let checks = check_models(&settings(ADDRESS), false).await;
        assert!(checks.iter().all(|c| c.name != "models"), "{checks:?}");
        let spare = format!(
            "{ADDRESS}[endpoints.spare]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\n[models.spare]\nendpoint = \"spare\"\nmodel = \"y\"\n"
        );
        let checks = check_models(&settings(&spare), false).await;
        let unused = checks.iter().find(|c| c.name == "models").unwrap();
        assert_eq!(
            unused.verdict,
            Verdict::Warn("configured but used by no role: spare".to_owned())
        );
    }

    const SIGNED_IN_AT: i64 = 1_790_000_000;
    const SECRETS: [&str; 3] = ["access-secret-1", "refresh-secret-1", "id-secret-1"];

    fn sign_in(refreshed_days_ago: i64, expires_in: i64) -> Credentials {
        Credentials {
            client_id: "oaiapp_x".to_owned(),
            ext_agent_host_id: "urn:uuid:1".to_owned(),
            access_token: SECRETS[0].to_owned().into(),
            refresh_token: SECRETS[1].to_owned().into(),
            id_token: Some(SECRETS[2].to_owned().into()),
            expires_at: SIGNED_IN_AT + expires_in,
            earliest_refresh_at: None,
            refreshed_at: SIGNED_IN_AT - refreshed_days_ago * 86_400,
            scopes: vec![
                "offline_access".to_owned(),
                "chatgpt.tokens.use.direct".to_owned(),
            ],
            account: Some("someone@example.com".to_owned()),
        }
    }

    fn verdict(check: &Check) -> (&'static str, &str) {
        match &check.verdict {
            Verdict::Ok(detail) => ("ok", detail),
            Verdict::Warn(detail) => ("warn", detail),
            Verdict::Fail(detail) => ("fail", detail),
        }
    }

    #[test]
    fn a_chatgpt_sign_in_is_described_by_its_file_alone() {
        let check = sign_in_check("c", &Ok((sign_in(3, 1800), Some(0o600))), SIGNED_IN_AT);
        let (kind, detail) = verdict(&check);
        assert_eq!(kind, "ok", "{detail}");
        assert!(detail.contains("someone@example.com"), "{detail}");
        assert!(detail.contains("valid for 30 min"), "{detail}");
        assert!(detail.contains("renewed 3 days ago"), "{detail}");
        for secret in SECRETS {
            assert!(!detail.contains(secret), "{detail}");
        }
        // An expired access token is fine: it is renewed on first use.
        let stale = sign_in_check("c", &Ok((sign_in(3, -50), Some(0o600))), SIGNED_IN_AT);
        assert_eq!(verdict(&stale).0, "ok");
        assert!(verdict(&stale).1.contains("renewed on first use"));
    }

    #[test]
    fn a_chatgpt_sign_in_that_needs_attention_says_what_to_do() {
        let at = |loaded: Result<(Credentials, Option<u32>), LoadError>| {
            let check = sign_in_check("c", &loaded, SIGNED_IN_AT);
            let (kind, detail) = verdict(&check);
            (kind, detail.to_owned())
        };
        let (kind, detail) = at(Err(LoadError::Missing("/v/openai.json".into())));
        assert_eq!(kind, "fail");
        assert!(
            detail.contains("not signed in") && detail.contains("henk auth openai --endpoint c"),
            "{detail}"
        );
        let (kind, detail) = at(Err(LoadError::Unusable {
            path: "/v/openai.json".into(),
            problem: "not JSON".to_owned(),
        }));
        assert_eq!(
            (kind, detail.contains("run `henk auth openai")),
            ("fail", true)
        );
        let mut without_scope = sign_in(1, 1800);
        without_scope.scopes = vec!["offline_access".to_owned()];
        let (kind, detail) = at(Ok((without_scope, Some(0o600))));
        assert_eq!(kind, "fail");
        assert!(detail.contains("without the scope"), "{detail}");
        let (kind, detail) = at(Ok((sign_in(31, 1800), Some(0o600))));
        assert_eq!(kind, "fail");
        assert!(detail.contains("most likely lapsed"), "{detail}");
        let (kind, detail) = at(Ok((sign_in(26, 1800), Some(0o600))));
        assert_eq!(kind, "warn");
        assert!(detail.contains("lapses soon"), "{detail}");
        let (kind, detail) = at(Ok((sign_in(2, 1800), Some(0o644))));
        assert_eq!(kind, "warn");
        assert!(
            detail.contains("readable by others") && detail.contains("644"),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn doctor_reads_a_chatgpt_endpoints_file_and_shows_no_token() {
        let dir = std::env::temp_dir().join(format!("henk-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("openai.json");
        let endpoints = format!(
            "[endpoints.c]\nprovider = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\nauth = \"chatgpt\"\nauth_file = \"{}\"\n[models.m]\nendpoint = \"c\"\nmodel = \"gpt-6.1-sol\"\n[roles.executor]\nmodel = \"m\"\nrequester_id = 3\n",
            file.display()
        );
        let settings = settings(&endpoints);

        // No file yet.
        let checks = check_models(&settings, false).await;
        let endpoint = checks.iter().find(|c| c.name == "endpoint c").unwrap();
        assert_eq!(verdict(endpoint).0, "fail");
        let model = checks.iter().find(|c| c.name == "model m").unwrap();
        assert!(verdict(model).1.contains("endpoint c is not usable"));

        // Signed in a day ago, the access token about to lapse.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let mut credentials = sign_in(0, 0);
        credentials.refreshed_at = now - 86_400;
        credentials.expires_at = now + 120;
        credentials.save(&file).unwrap();
        let checks = check_models(&settings, false).await;
        let endpoint = checks.iter().find(|c| c.name == "endpoint c").unwrap();
        assert_eq!(verdict(endpoint).0, "ok", "{endpoint:?}");
        // With --probe it does not renew from here, and says why.
        let probed = check_models(&settings, true).await;
        let endpoint = probed.iter().find(|c| c.name == "endpoint c").unwrap();
        assert_eq!(verdict(endpoint).0, "warn", "{endpoint:?}");
        assert!(verdict(endpoint).1.contains("no live call from here"));
        let rendered = render(&probed);
        for secret in SECRETS {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A missing key fails its endpoint once, and each model on it says
    /// so; an endpoint no model uses is a warning (#368).
    #[tokio::test]
    async fn an_endpoint_is_checked_once_and_its_models_follow_it() {
        let text = "[endpoints.e]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"HENK_DOCTOR_TEST_UNSET_KEY\"\n[endpoints.idle]\nprovider = \"anthropic\"\nbase_url = \"https://y.test\"\napi_key_env = \"HENK_DOCTOR_TEST_UNSET_KEY\"\n[models.m]\nendpoint = \"e\"\nmodel = \"x\"\n[models.n]\nendpoint = \"e\"\nmodel = \"y\"\n[roles.executor]\nmodel = \"m\"\nrequester_id = 3\n";
        let checks = check_models(&settings(text), false).await;
        let verdict = |name: &str| {
            checks
                .iter()
                .find(|c| c.name == name)
                .map_or_else(|| panic!("no {name} in {checks:?}"), |c| c.verdict.clone())
        };
        assert_eq!(
            verdict("endpoint e"),
            Verdict::Fail("environment variable HENK_DOCTOR_TEST_UNSET_KEY is not set".to_owned())
        );
        assert_eq!(
            checks
                .iter()
                .filter(|c| c.name.starts_with("endpoint e"))
                .count(),
            1,
            "the endpoint once, not once per model"
        );
        for model in ["model m", "model n"] {
            assert_eq!(
                verdict(model),
                Verdict::Fail("its endpoint e is not usable (see above)".to_owned())
            );
        }
        assert_eq!(
            verdict("endpoints"),
            Verdict::Warn("configured but used by no model: idle".to_owned())
        );
    }

    #[tokio::test]
    async fn git_missing_from_henk_s_own_path_fails_and_says_why_it_is_needed() {
        let nowhere = std::ffi::OsStr::new("/nonexistent/henk-doctor-test");
        let check = check_git(&settings(ADDRESS), Some(nowhere)).await;
        let Verdict::Fail(text) = &check.verdict else {
            panic!("{check:?}")
        };
        assert_eq!(check.name, "git");
        assert!(text.starts_with("git could not run: "), "{text}");
        assert!(text.contains("needed for the executor's pushes"), "{text}");
        assert!(text.contains("install git in Henk's image"), "{text}");
        assert!(check.is_failure());
    }

    #[tokio::test]
    async fn git_on_the_path_is_fine_and_unneeded_git_is_not_run() {
        let check = check_git(&settings(ADDRESS), None).await;
        let Verdict::Ok(text) = &check.verdict else {
            panic!("git is on the test machine's PATH: {check:?}")
        };
        assert!(text.starts_with("git version "), "{text}");
        let nowhere = std::ffi::OsStr::new("/nonexistent/henk-doctor-test");
        let unneeded = check_git(&settings(""), Some(nowhere)).await;
        assert_eq!(
            unneeded.verdict,
            Verdict::Ok(
                "not needed: no profile checks out a repository and nothing pushes".to_owned()
            )
        );
    }

    #[test]
    fn mise_is_wanted_on_the_sandbox_host_only_by_a_profile_there() {
        use henk_domain::workspace::Profile;
        let report = "henk-sandbox 4\ngit /usr/bin/git\nmise missing\n";
        let profile = |backend, toolchain| Profile {
            backend,
            toolchain,
            ..Profile::default()
        };
        let mut workspace = WorkspacePolicy::default();
        workspace.profiles.insert(
            "local".to_owned(),
            profile(BackendKind::Host, Some(Toolchain::Mise)),
        );
        workspace
            .profiles
            .insert("sandbox".to_owned(), profile(BackendKind::Ssh, None));
        assert_eq!(
            tools_check(
                "sandbox host tools",
                report,
                &workspace,
                BackendKind::Ssh,
                None
            )
            .verdict,
            Verdict::Ok("henk-sandbox 4".to_owned()),
            "only a host-backed profile wants mise"
        );
        workspace.profiles.insert(
            "sandbox".to_owned(),
            profile(BackendKind::Ssh, Some(Toolchain::Mise)),
        );
        assert_eq!(
            tools_check(
                "sandbox host tools",
                report,
                &workspace,
                BackendKind::Ssh,
                None
            )
            .verdict,
            Verdict::Fail("henk-sandbox 4; missing there: mise".to_owned()),
            "a sandbox profile wants it"
        );
    }

    #[test]
    fn henk_needs_pods_and_exec_in_the_sandbox_and_no_secrets() {
        let answer = |secrets: bool, exec: bool| {
            vec![
                ("create pods".to_owned(), true),
                ("create pods/exec".to_owned(), exec),
                ("get secrets".to_owned(), secrets),
            ]
        };
        assert!(matches!(
            access_check("box", &answer(false, true)).verdict,
            Verdict::Ok(_)
        ));
        let Verdict::Fail(why) = access_check("box", &answer(false, false)).verdict else {
            panic!("exec is needed")
        };
        assert!(
            why.contains("create pods/exec") && why.contains("sandbox-namespace.yaml"),
            "{why}"
        );
        let Verdict::Fail(why) = access_check("box", &answer(true, true)).verdict else {
            panic!("secrets are too much")
        };
        assert!(why.contains("Secrets"), "{why}");
        assert!(henk_domain::text::is_in_style(&why));
    }

    #[test]
    fn render_counts_failures() {
        let checks = vec![
            Check::ok("a", "fine"),
            Check::warn("b", "hm"),
            Check::fail("c", "no"),
        ];
        let text = render(&checks);
        assert!(text.contains("[ok  ] a: fine"));
        assert!(text.contains("[FAIL] c: no"));
        assert!(text.ends_with("1 failing check(s)"));
    }

    #[test]
    fn a_missing_gitlab_token_warns_only_when_address_runs_need_it() {
        let unset = "HENK_TEST_GITLAB_TOKEN_THAT_IS_NEVER_SET";
        assert_eq!(
            gitlab_token(unset, true).verdict,
            Verdict::Warn("not set; reviews on GitLab cannot push".to_owned())
        );
        assert!(matches!(gitlab_token(unset, false).verdict, Verdict::Ok(_)));
    }

    #[test]
    fn the_dashboard_warns_when_it_signs_in_through_a_github_app() {
        assert!(matches!(
            dashboard_client_id("V", Some("Iv23liAbCdEfGhIjKlMn")).verdict,
            Verdict::Warn(text) if text.contains("act on their behalf")
        ));
        assert!(matches!(
            dashboard_client_id("V", Some("Ov23liAbCdEfGhIjKlMn")).verdict,
            Verdict::Ok(_)
        ));
        assert_eq!(
            dashboard_client_id("V", None).verdict,
            Verdict::Warn("not set; the dashboard is off".to_owned())
        );
    }

    fn sandbox_pool(extra: &str) -> Settings {
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[1; 32]),
        )
        .public_key()
        .to_openssh()
        .unwrap();
        // PATH is always set: the key's variable counts as set.
        settings(&format!(
            "[workspace.ssh]\nkey_path_env = \"PATH\"\n[workspace.ssh.hosts.goat]\nhost = \"g.example\"\nhost_key = \"{key}\"\n[workspace.ssh.hosts.fox]\nhost = \"f.example\"\nhost_key = \"{key}\"\n{extra}"
        ))
    }

    #[tokio::test]
    async fn each_sandbox_host_is_checked_under_its_own_name() {
        let names: Vec<String> = check_sandbox(&sandbox_pool(""), false)
            .await
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            ["secret $PATH", "sandbox fox host", "sandbox goat host"],
            "the shared key once, then each host"
        );
    }

    #[test]
    fn mise_is_wanted_on_a_sandbox_host_only_by_a_profile_that_may_use_it() {
        let settings = sandbox_pool(
            "[workspace.profiles.big]\nbackend = \"ssh\"\ntoolchain = \"mise\"\nhosts = [\"goat\"]\n",
        );
        let report = "henk-sandbox 5\nmise missing\n";
        let tools = |host| {
            tools_check(
                "tools",
                report,
                &settings.workspace,
                BackendKind::Ssh,
                Some(host),
            )
            .verdict
        };
        assert!(
            matches!(tools("goat"), Verdict::Fail(_)),
            "big may use goat"
        );
        assert_eq!(
            tools("fox"),
            Verdict::Ok("henk-sandbox 5".to_owned()),
            "no profile that wants mise may use fox"
        );
    }

    #[test]
    fn the_probe_leaves_a_reasoning_model_its_cap() {
        let endpoints = |api: &str| {
            format!(
                "[endpoints.o]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\n{api}[models.m]\nendpoint = \"o\"\nmodel = \"x\"\n"
            )
        };
        let cap = |api: &str| {
            let settings = settings(&endpoints(api));
            probe_cap(&settings, settings.models.get("m").unwrap())
        };
        assert_eq!(cap(""), Some(16), "Chat Completions answers in 16");
        assert_eq!(cap("api = \"chat_completions\"\n"), Some(16));
        assert_eq!(cap("api = \"responses\"\n"), None);
    }

    #[test]
    fn allowlist_entries_no_installation_reaches_are_warned_about() {
        let settings = settings("");
        let mut allowlist = settings.allowlist.rules().to_vec();
        let github = henk_domain::allowlist::Platform::GitHub;
        let repo = |path| {
            henk_domain::allowlist::AllowRule::Repository(
                henk_domain::allowlist::RepoRef::parse(github, path).unwrap(),
            )
        };
        allowlist.extend([
            henk_domain::allowlist::AllowRule::Owner {
                platform: github,
                owner: "nobody".to_owned(),
            },
            repo("Alice/x"),
            repo("alice/hidden"),
            repo("stranger/y"),
        ]);
        let allowlist = henk_domain::allowlist::Allowlist::new(allowlist);
        let installation = |id, account: &str| Installation {
            id,
            account: account.to_owned(),
        };
        let usable = [
            (installation(111, "alice"), Some(vec!["alice/x".to_owned()])),
            (installation(222, "DocSpec"), None),
        ];
        let warned: Vec<String> = uncovered(&allowlist, &usable)
            .into_iter()
            .map(|check| match check.verdict {
                Verdict::Warn(why) => why,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            warned,
            [
                "github_owners nobody: no usable installation of the App on nobody; Henk cannot work there",
                "github_repositories alice/hidden: the App's installation on alice does not include it",
                "github_repositories stranger/y: no usable installation of the App on stranger; Henk cannot work there",
            ],
            "docspec is covered whatever its repositories, alice/x by its list"
        );
    }
}
