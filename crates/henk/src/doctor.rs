//! `henk doctor`: checks a deployment before the first webhook arrives.
//!
//! Every check is independent and reported on its own line, so one missing
//! secret does not hide the next problem.

use std::fmt::Write as _;
use std::time::Duration;

use henk_domain::workspace::{BackendKind, Toolchain, WorkspacePolicy};
use henk_mcp::{McpSession as _, RmcpSession};
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth};

use crate::app::env_var;
use crate::config::{DatabaseConfig, Settings};

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
    checks.extend(check_github(settings).await);
    checks.extend(check_mcp(settings).await);
    checks.extend(check_mcp_server(settings, probe_models, env_var).await);
    checks.push(check_database(settings).await);
    checks.extend(check_sandbox(settings, probe_models).await);
    checks.extend(check_kubernetes(settings, probe_models).await);
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

/// The `ssh` backend's sandbox host (#84): without `--probe`, only whether
/// its key is set; with it, the connection, the host key and the tools the
/// sandbox script needs there, each on a line of its own.
async fn check_sandbox(settings: &Settings, probe: bool) -> Vec<Check> {
    let Some(ssh) = &settings.workspace_ssh else {
        return Vec::new();
    };
    let target = format!("{}@{}:{}", ssh.user, ssh.host, ssh.port);
    let key = format!("secret ${}", ssh.key_path_env);
    if env_var(&ssh.key_path_env).is_none() {
        return vec![Check::fail(
            key,
            "not set; the ssh workspace backend cannot sign in",
        )];
    }
    if !probe {
        return vec![
            Check::ok(key, "set; Henk's key for the sandbox host"),
            Check::ok("sandbox host", format!("{target}; --probe connects")),
        ];
    }
    let provider = match crate::workspace::ssh_provider(settings) {
        Ok(Some(provider)) => provider,
        Ok(None) => return Vec::new(),
        Err(error) => return vec![Check::fail(key, format!("{error:#}"))],
    };
    match provider.probe().await {
        Ok(report) => vec![
            Check::ok("sandbox connection", format!("{target}, signed in")),
            Check::ok("sandbox host key", "matches workspace.ssh.host_key"),
            tools_check(
                "sandbox host tools",
                &report,
                &settings.workspace,
                BackendKind::Ssh,
            ),
        ],
        Err(error) => {
            let text = error.to_string();
            let refused_key = text.contains("pinned workspace.ssh.host_key");
            vec![
                Check::fail("sandbox connection", format!("{target}: {text}")),
                if refused_key {
                    Check::fail("sandbox host key", "not the pinned workspace.ssh.host_key")
                } else {
                    Check::warn("sandbox host key", "not checked: no connection")
                },
            ]
        }
    }
}

/// The sandbox script's probe report as a check named `name`: a tool it
/// says is missing fails it, mise only when a profile on `backend` names it
/// as its toolchain; a host-backed profile runs mise on Henk's own machine.
fn tools_check(
    name: &str,
    report: &str,
    workspace: &WorkspacePolicy,
    backend: BackendKind,
) -> Check {
    let mut lines = report.lines();
    let version = lines.next().unwrap_or("henk-sandbox (no version)");
    let wants_mise = workspace
        .named()
        .any(|(_, p)| p.backend == backend && p.toolchain == Some(Toolchain::Mise));
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
            Ok(report) => tools_check(&name, &report, &settings.workspace, BackendKind::Kubernetes),
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
        checks.push(gitlab_token(&gitlab.token_env, settings.address.is_some()));
    }
    checks
}

/// Henk reads the GitLab token himself only for address runs (§3.5): to
/// push, and for the REST reads the MCP server does not give. Set or not
/// set, never its value.
fn gitlab_token(variable: &str, address: bool) -> Check {
    let name = format!("secret ${variable}");
    match (env_var(variable).is_some(), address) {
        (true, true) => Check::ok(name, "set; GitLab address runs can push"),
        (true, false) => Check::ok(name, "set; address runs are not configured"),
        (false, true) => Check::warn(name, "not set; GitLab address runs are refused"),
        (false, false) => Check::ok(name, "not set; address runs are not configured anyway"),
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
    for (id, model) in &settings.models {
        let name = format!("model {id}");
        let config = match model.to_client_config(env_var) {
            Ok(config) => config,
            Err(variable) => {
                checks.push(Check::fail(
                    name,
                    format!("environment variable {variable} is not set"),
                ));
                continue;
            }
        };
        if !probe {
            checks.push(Check::ok(
                name,
                format!(
                    "{} at {} (key present; use --probe to call it)",
                    model.model, model.base_url
                ),
            ));
            continue;
        }
        let client = match henk_llm::client_for(config) {
            Ok(client) => client,
            Err(error) => {
                checks.push(Check::fail(name, error.to_string()));
                continue;
            }
        };
        let request = henk_llm::CompletionRequest {
            messages: vec![henk_llm::ChatMessage::user(
                "Reply with the single word: ready",
            )],
            // A thinking model spends its cap on thinking first; give it
            // the configured one rather than cutting it off before it answers.
            max_tokens: model.effort.is_none().then_some(16),
            ..Default::default()
        };
        match tokio::time::timeout(Duration::from_mins(1), client.complete(&request)).await {
            Ok(Ok(completion)) => checks.push(Check::ok(
                name,
                format!("answered: {:?}", completion.message.text().trim()),
            )),
            Ok(Err(error)) => checks.push(Check::fail(name, error.to_string())),
            Err(_) => checks.push(Check::fail(name, "no answer within 60s")),
        }
    }
    let unused: Vec<&str> = settings
        .models
        .keys()
        .filter(|id| {
            !settings
                .lanes
                .iter()
                .any(|l| l.model.as_str() == id.as_str())
                && settings.planning.as_ref().is_none_or(|p| &p.model != *id)
                && settings
                    .review
                    .fact_check
                    .as_ref()
                    .is_none_or(|f| &f.model != *id && f.backup_model.as_ref() != Some(*id))
        })
        .map(String::as_str)
        .collect();
    if !unused.is_empty() {
        checks.push(Check::warn(
            "models",
            format!(
                "configured but used by no lane, fact-check or planner: {}",
                unused.join(", ")
            ),
        ));
    }
    if settings.lanes.is_empty() {
        checks.push(Check::warn(
            "review lanes",
            "none configured; reviews cannot run",
        ));
    }
    checks
}

async fn check_github(settings: &Settings) -> Vec<Check> {
    let Some(github) = &settings.github else {
        return vec![Check::ok("GitHub", "not configured")];
    };
    let Some(path) = env_var(&github.private_key_path_env) else {
        return vec![Check::fail(
            "GitHub App key",
            format!(
                "environment variable {} is not set",
                github.private_key_path_env
            ),
        )];
    };
    let pem = match std::fs::read_to_string(&path) {
        Ok(pem) => pem,
        Err(error) => {
            return vec![Check::fail(
                "GitHub App key",
                format!("cannot read {path}: {error}"),
            )];
        }
    };
    let credentials = match AppCredentials::from_pem(github.app_id, github.installation_id, &pem) {
        Ok(credentials) => credentials,
        Err(error) => return vec![Check::fail("GitHub App key", error.to_string())],
    };
    let api = match GitHubApi::new(&github.api_base, GitHubAuth::app(credentials)) {
        Ok(api) => api,
        Err(error) => return vec![Check::fail("GitHub client", error.to_string())],
    };
    let mut checks = vec![check_app_identity(&api, &github.bot_login).await];
    if github.installation_id == 0 {
        checks.push(Check::fail(
            "GitHub App installation",
            format!(
                "installation_id is 0; set it to one of these: {}",
                list_installations(&api).await
            ),
        ));
        return checks;
    }
    match tokio::time::timeout(
        Duration::from_secs(30),
        api.get("/installation/repositories?per_page=1"),
    )
    .await
    {
        Ok(Ok(value)) => {
            let total = value
                .get("total_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            checks.push(Check::ok(
                "GitHub App installation",
                format!(
                    "installation {} token works; {total} repositories visible",
                    github.installation_id
                ),
            ));
        }
        Ok(Err(error)) => checks.push(Check::fail(
            "GitHub App installation",
            format!(
                "installation {} token failed: {error}; the App is installed as: {}",
                github.installation_id,
                list_installations(&api).await
            ),
        )),
        Err(_) => checks.push(Check::fail(
            "GitHub App installation",
            "no answer within 30s",
        )),
    }
    checks
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

/// The installations of the App, as text for a check detail.
async fn list_installations(api: &GitHubApi) -> String {
    match tokio::time::timeout(
        Duration::from_secs(30),
        api.get_as_app("/app/installations"),
    )
    .await
    {
        Ok(Ok(serde_json::Value::Array(installations))) if !installations.is_empty() => {
            installations
                .iter()
                .map(|i| {
                    let id = i.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0);
                    let account = i
                        .get("account")
                        .and_then(|a| a.get("login"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let selection = i
                        .get("repository_selection")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    format!("{id} ({account}, {selection} repositories)")
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        Ok(Ok(_)) => "none; install the App on an account first".to_owned(),
        Ok(Err(error)) => format!("could not list installations: {error}"),
        Err(_) => "could not list installations within 30s".to_owned(),
    }
}

async fn check_mcp(settings: &Settings) -> Vec<Check> {
    let mut checks = Vec::new();
    for (alias, config) in &settings.mcp {
        let name = format!("MCP server {alias}");
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
            tools_check("sandbox host tools", report, &workspace, BackendKind::Ssh).verdict,
            Verdict::Ok("henk-sandbox 4".to_owned()),
            "only a host-backed profile wants mise"
        );
        workspace.profiles.insert(
            "sandbox".to_owned(),
            profile(BackendKind::Ssh, Some(Toolchain::Mise)),
        );
        assert_eq!(
            tools_check("sandbox host tools", report, &workspace, BackendKind::Ssh).verdict,
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
            Verdict::Warn("not set; GitLab address runs are refused".to_owned())
        );
        assert!(matches!(gitlab_token(unset, false).verdict, Verdict::Ok(_)));
    }
}
