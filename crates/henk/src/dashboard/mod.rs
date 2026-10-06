//! The web dashboard (#36): what Henk is doing and has done, behind a GitHub
//! sign-in. Read-only: nothing here starts, stops or changes a run.

mod auth;
mod session;
mod views;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use secrecy::SecretString;

use crate::app::{App, env_var};
use crate::config::DashboardConfig;
use crate::coordinator::Coordinator;
use session::Signer;

/// Everything the dashboard's handlers share.
pub struct Dashboard {
    app: Arc<App>,
    coordinator: Arc<Coordinator>,
    listeners: Vec<&'static str>,
    config: DashboardConfig,
    client_id: String,
    client_secret: SecretString,
    signer: Signer,
    http: reqwest::Client,
}

impl std::fmt::Debug for Dashboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dashboard").finish_non_exhaustive()
    }
}

/// The shortest session key accepted.
const MIN_KEY_BYTES: usize = 32;

/// The dashboard's secrets, read from the variables the config names.
#[derive(Debug)]
pub struct DashboardSecrets {
    /// The GitHub OAuth App's client id.
    pub client_id: String,
    /// Its client secret.
    pub client_secret: SecretString,
    /// The key that signs sessions.
    pub session_key: SecretString,
}

impl DashboardSecrets {
    /// Reads the secrets, or says which one is missing or too short.
    ///
    /// # Errors
    ///
    /// Returns the reason the dashboard cannot run.
    pub fn from_env(config: &DashboardConfig) -> Result<Self, String> {
        let read = |name: &str| env_var(name).ok_or_else(|| format!("{name} is not set"));
        let session_key = read(&config.session_key_env)?;
        if session_key.len() < MIN_KEY_BYTES {
            return Err(format!(
                "{} is shorter than {MIN_KEY_BYTES} bytes",
                config.session_key_env
            ));
        }
        Ok(Self {
            client_id: read(&config.client_id_env)?,
            client_secret: SecretString::from(read(&config.client_secret_env)?),
            session_key: SecretString::from(session_key),
        })
    }
}

impl Dashboard {
    /// Builds the dashboard over the running app.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client for GitHub cannot be built.
    pub fn new(
        app: Arc<App>,
        coordinator: Arc<Coordinator>,
        listeners: Vec<&'static str>,
        config: DashboardConfig,
        secrets: &DashboardSecrets,
    ) -> anyhow::Result<Self> {
        use secrecy::ExposeSecret as _;
        henk_llm::ensure_tls_provider();
        let secure = app.settings.server.public_base_url.starts_with("https://");
        let http = reqwest::Client::builder()
            .user_agent("meneer-henk-dashboard")
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self {
            signer: Signer::new(secrets.session_key.expose_secret().as_bytes(), secure),
            client_id: secrets.client_id.clone(),
            client_secret: secrets.client_secret.clone(),
            app,
            coordinator,
            listeners,
            config,
            http,
        })
    }

    /// The callback GitHub sends people back to.
    fn redirect_uri(&self) -> String {
        format!(
            "{}/dashboard/auth/callback",
            self.app
                .settings
                .server
                .public_base_url
                .trim_end_matches('/')
        )
    }
}

/// Every dashboard route.
pub fn routes(dashboard: Arc<Dashboard>) -> Router {
    Router::new()
        .route("/dashboard", get(views::overview))
        .route("/dashboard/runs/{id}", get(views::run))
        .route("/dashboard/events", get(views::events))
        .route("/dashboard/events/{id}", get(views::event))
        .route("/dashboard/health", get(views::health))
        .route("/dashboard/running.json", get(views::running_json))
        .route("/dashboard/app.js", get(views::app_js))
        .route("/dashboard/login", get(auth::login))
        .route("/dashboard/auth/callback", get(auth::callback))
        .route("/dashboard/logout", post(auth::logout))
        .with_state(dashboard)
}

#[cfg(test)]
mod tests;
