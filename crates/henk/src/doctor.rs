//! `henk doctor`: checks a deployment before the first webhook arrives.
//!
//! Every check is independent and reported on its own line, so one missing
//! secret does not hide the next problem.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use henk_mcp::{McpSession as _, RmcpSession};
use henk_platform::github::{AppCredentials, GitHubApi, GitHubAuth};

use crate::app::env_var;
use crate::config::Settings;

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
    checks.push(check_database(settings));
    checks
}

fn check_secrets(settings: &Settings) -> Vec<Check> {
    let server = &settings.server;
    [
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
    .collect()
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

fn check_database(settings: &Settings) -> Check {
    let path = Path::new(&settings.server.database_path);
    match henk_store::RunStore::open(path) {
        Ok(_) => Check::ok(
            "database",
            format!("{} opens and is migrated", path.display()),
        ),
        Err(error) => Check::fail("database", format!("{}: {error}", path.display())),
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
}
