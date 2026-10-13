//! GitHub App authentication without a JWT crate: RS256 on ring.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::debug;

use crate::error::PlatformError;
use crate::retry::{RetryPolicy, with_retry};

/// The App's identity and private key.
pub struct AppCredentials {
    app_id: u64,
    key: RsaKeyPair,
}

impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials")
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

impl AppCredentials {
    /// Loads the PEM private key GitHub issues for an App. Both the PKCS#1
    /// form GitHub downloads (`BEGIN RSA PRIVATE KEY`) and PKCS#8
    /// (`BEGIN PRIVATE KEY`) are accepted.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] when the PEM cannot be read.
    pub fn from_pem(app_id: u64, pem: &str) -> Result<Self, PlatformError> {
        let (label, der) = decode_pem(pem)?;
        let key = match label {
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
            other => {
                return Err(PlatformError::Auth(format!(
                    "unsupported PEM label {other:?}"
                )));
            }
        }
        .map_err(|error| PlatformError::Auth(format!("unreadable App private key: {error}")))?;
        Ok(Self { app_id, key })
    }

    /// The App's id.
    #[must_use]
    pub const fn app_id(&self) -> u64 {
        self.app_id
    }

    /// A signed App JWT valid for about nine minutes, backdated a minute
    /// against clock skew, as GitHub's documentation prescribes.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] when signing fails.
    pub fn jwt(&self, now: SystemTime) -> Result<String, PlatformError> {
        let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({
            "iat": now.saturating_sub(60),
            "exp": now + 9 * 60,
            "iss": self.app_id.to_string(),
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| PlatformError::Auth("JWT signing failed".to_owned()))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

fn decode_pem(pem: &str) -> Result<(&str, Vec<u8>), PlatformError> {
    let pem = pem.trim();
    let start = pem
        .find("-----BEGIN ")
        .ok_or_else(|| PlatformError::Auth("no PEM header".to_owned()))?;
    let after = pem.get(start + 11..).unwrap_or_default();
    let label_end = after
        .find("-----")
        .ok_or_else(|| PlatformError::Auth("bad PEM header".to_owned()))?;
    let label = after.get(..label_end).unwrap_or_default();
    let body_start = label_end + 5;
    let footer = format!("-----END {label}-----");
    let body_end = after
        .find(&footer)
        .ok_or_else(|| PlatformError::Auth("no PEM footer".to_owned()))?;
    let body: String = after
        .get(body_start..body_end)
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|error| PlatformError::Auth(format!("PEM body is not base64: {error}")))?;
    Ok((label, der))
}

/// The installations of one App that Henk may act for, and the tokens it
/// holds for them. One per process, shared by every client: a repository's
/// installation is looked up once per owner, and each installation's token
/// is minted once and renewed before its hour runs out.
pub struct Installations {
    credentials: AppCredentials,
    /// The ceiling from the configuration; `None` is every installation.
    allowed: Option<BTreeSet<u64>>,
    /// Owner, lowercased, to installation id.
    owners: Mutex<HashMap<String, u64>>,
    /// Each installation's token behind its own lock. That lock is held
    /// across a mint and its retries, so one installation's token is minted
    /// once even when several calls want it at the same moment; the map's
    /// lock is held only to find the slot, so a mint that is waiting to try
    /// again holds up no other installation.
    tokens: tokio::sync::Mutex<HashMap<u64, Arc<TokenSlot>>>,
}

impl std::fmt::Debug for Installations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Installations")
            .field("credentials", &self.credentials)
            .field("allowed", &self.allowed)
            .finish_non_exhaustive()
    }
}

impl Installations {
    /// The installations of the App `credentials` names, limited to
    /// `allowed` when that is `Some`.
    #[must_use]
    pub fn new(credentials: AppCredentials, allowed: Option<BTreeSet<u64>>) -> Self {
        Self {
            credentials,
            allowed,
            owners: Mutex::new(HashMap::new()),
            tokens: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The App's id.
    #[must_use]
    pub const fn app_id(&self) -> u64 {
        self.credentials.app_id
    }

    /// Whether the configuration lets Henk act for installation `id`.
    #[must_use]
    pub fn allows(&self, id: u64) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(&id))
    }

    /// Records that `owner`'s repositories belong to installation `id`, as
    /// a webhook delivery says, so work it starts needs no lookup.
    pub fn remember(&self, owner: &str, id: u64) {
        if let Ok(mut owners) = self.owners.lock() {
            owners.insert(owner.to_lowercase(), id);
        }
    }

    /// Drops every owner remembered for installation `id`.
    fn forget(&self, id: u64) {
        if let Ok(mut owners) = self.owners.lock() {
            owners.retain(|_, remembered| *remembered != id);
        }
    }

    fn known(&self, owner: &str) -> Option<u64> {
        self.owners
            .lock()
            .ok()
            .and_then(|owners| owners.get(&owner.to_lowercase()).copied())
    }

    /// The installation that reaches `owner/name`: remembered, or asked of
    /// GitHub with the App JWT and then remembered for the owner.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::NotInstalled`] when the App is not installed
    /// there or the installation is outside the configured ones, otherwise
    /// as the request fails; a failure that was still worth another try
    /// once `retry` ran out is [`PlatformError::Auth`], as for
    /// [`Self::token`].
    pub async fn installation_for(
        &self,
        http: &reqwest::Client,
        api_base: &str,
        owner: &str,
        name: &str,
        retry: RetryPolicy,
    ) -> Result<u64, PlatformError> {
        let id = if let Some(id) = self.known(owner) {
            id
        } else {
            let what = format!("GET /repos/{owner}/{name}/installation");
            let id = with_retry(retry, &what, || self.look_up(http, api_base, owner, name))
                .await
                .map_err(|error| settled(&what, error))?;
            debug!(owner, installation = id, "found installation");
            self.remember(owner, id);
            id
        };
        if !self.allows(id) {
            return Err(PlatformError::NotInstalled {
                repo: format!("{owner}/{name}"),
                reason: format!("installation {id} is not one of [github] installations"),
            });
        }
        Ok(id)
    }

    async fn look_up(
        &self,
        http: &reqwest::Client,
        api_base: &str,
        owner: &str,
        name: &str,
    ) -> Result<u64, PlatformError> {
        let jwt = self.credentials.jwt(SystemTime::now())?;
        let response = http
            .get(format!("{api_base}/repos/{owner}/{name}/installation"))
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(PlatformError::NotInstalled {
                repo: format!("{owner}/{name}"),
                reason: "the App is not installed there".to_owned(),
            });
        }
        if !status.is_success() {
            return Err(PlatformError::status(
                status.as_u16(),
                &body,
                Some(&headers),
            ));
        }
        let value: Value =
            serde_json::from_str(&body).map_err(|e| PlatformError::Decode(e.to_string()))?;
        value
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| PlatformError::Decode("installation response without id".to_owned()))
    }

    /// A token for installation `id`, minted when there is none or the one
    /// held is within two minutes of expiry.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::NotInstalled`] for an installation outside
    /// the configured ones, otherwise as the token request fails; a failure
    /// that was still worth another try once `retry` ran out is
    /// [`PlatformError::Auth`], so the request around it does not try the
    /// mint again.
    pub async fn token(
        &self,
        http: &reqwest::Client,
        api_base: &str,
        id: u64,
        retry: RetryPolicy,
    ) -> Result<SecretString, PlatformError> {
        if !self.allows(id) {
            return Err(PlatformError::NotInstalled {
                repo: format!("installation {id}"),
                reason: format!("installation {id} is not one of [github] installations"),
            });
        }
        let slot = Arc::clone(self.tokens.lock().await.entry(id).or_default());
        let mut held = slot.lock().await;
        let now = SystemTime::now();
        if let Some(cached) = held.as_ref()
            && cached.expires_at > now + Duration::from_mins(2)
        {
            return Ok(cached.token.clone());
        }
        let what = format!("POST /app/installations/{id}/access_tokens");
        let minted = with_retry(retry, &what, || {
            mint_installation_token(http, api_base, &self.credentials, id, SystemTime::now())
        })
        .await;
        let fresh = match minted {
            Ok(fresh) => fresh,
            Err(error) => {
                if matches!(
                    error,
                    PlatformError::Status {
                        status: 401 | 403 | 404,
                        ..
                    }
                ) {
                    // The installation is gone or changed (uninstalled and
                    // reinstalled gets a new id): forget the owners that
                    // pointed at it, so the next call looks them up again.
                    self.forget(id);
                }
                return Err(settled(&what, error));
            }
        };
        let token = fresh.token.clone();
        *held = Some(fresh);
        Ok(token)
    }
}

/// One installation's token, if minted yet.
type TokenSlot = tokio::sync::Mutex<Option<CachedToken>>;

/// The error of an auth step (a token mint, an installation lookup) whose
/// own retries ran out. One that another try might still fix becomes
/// [`PlatformError::Auth`], which is not retryable: the request that needed
/// the token runs inside its own retry loop, and repeating the whole step
/// there would multiply the attempts (`max_attempts` squared mints for one
/// read).
fn settled(what: &str, error: PlatformError) -> PlatformError {
    if error.is_retryable() {
        PlatformError::Auth(format!("{what} kept failing: {error}"))
    } else {
        error
    }
}

/// Which installation a client's calls go out as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// None yet: only App (JWT) calls work. The client Henk builds once and
    /// scopes per repository.
    Unscoped,
    /// The installation that reaches this repository.
    Repository {
        /// Its owner.
        owner: String,
        /// Its name.
        name: String,
    },
    /// This installation.
    Installation(u64),
}

/// How the client authenticates to GitHub.
pub enum GitHubAuth {
    /// A GitHub App, acting for the installation its scope names.
    App {
        /// The App's installations and their tokens, shared.
        installations: Arc<Installations>,
        /// Which installation this client acts for.
        scope: Scope,
        /// The scope's installation, once found; fixed for the client's
        /// life.
        resolved: tokio::sync::OnceCell<u64>,
    },
    /// A fixed token (a PAT, or a test value).
    Token(SecretString),
}

/// An installation token and when it expires.
#[derive(Debug, Clone)]
pub struct CachedToken {
    token: SecretString,
    expires_at: SystemTime,
}

impl std::fmt::Debug for GitHubAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::App {
                installations,
                scope,
                ..
            } => f
                .debug_struct("App")
                .field("installations", installations)
                .field("scope", scope)
                .finish_non_exhaustive(),
            Self::Token(_) => f.write_str("Token(..)"),
        }
    }
}

impl GitHubAuth {
    /// App authentication, not yet scoped to an installation.
    #[must_use]
    pub fn app(installations: Arc<Installations>) -> Self {
        Self::App {
            installations,
            scope: Scope::Unscoped,
            resolved: tokio::sync::OnceCell::new(),
        }
    }

    /// A fixed token.
    #[must_use]
    pub fn token(token: SecretString) -> Self {
        Self::Token(token)
    }

    /// The same authentication acting for `scope`. A fixed token stays
    /// itself.
    #[must_use]
    pub fn scoped(&self, scope: Scope) -> Self {
        match self {
            Self::Token(token) => Self::Token(token.clone()),
            Self::App { installations, .. } => Self::App {
                installations: Arc::clone(installations),
                resolved: match scope {
                    Scope::Installation(id) => tokio::sync::OnceCell::new_with(Some(id)),
                    Scope::Unscoped | Scope::Repository { .. } => tokio::sync::OnceCell::new(),
                },
                scope,
            },
        }
    }

    /// The App's installations, for an App.
    #[must_use]
    pub fn installations(&self) -> Option<&Arc<Installations>> {
        match self {
            Self::App { installations, .. } => Some(installations),
            Self::Token(_) => None,
        }
    }

    /// A signed App JWT, for the few endpoints that authenticate as the App
    /// itself rather than as an installation (`GET /app`,
    /// `GET /app/installations`). `None` for a fixed token.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] when signing fails.
    pub fn app_jwt(&self) -> Option<Result<SecretString, PlatformError>> {
        match self {
            Self::Token(_) => None,
            Self::App { installations, .. } => Some(
                installations
                    .credentials
                    .jwt(SystemTime::now())
                    .map(SecretString::from),
            ),
        }
    }

    /// The installation this client acts for, found on first use.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError::Auth`] for an unscoped App client and a
    /// fixed token, otherwise as [`Installations::installation_for`].
    pub async fn installation(
        &self,
        http: &reqwest::Client,
        api_base: &str,
        retry: RetryPolicy,
    ) -> Result<u64, PlatformError> {
        match self {
            Self::Token(_) => Err(PlatformError::Auth(
                "a fixed token has no installation".to_owned(),
            )),
            Self::App {
                installations,
                scope,
                resolved,
            } => resolved
                .get_or_try_init(|| async {
                    match scope {
                        Scope::Installation(id) => Ok(*id),
                        Scope::Repository { owner, name } => {
                            installations
                                .installation_for(http, api_base, owner, name, retry)
                                .await
                        }
                        Scope::Unscoped => Err(PlatformError::Auth(
                            "no repository or installation chosen for this GitHub call".to_owned(),
                        )),
                    }
                })
                .await
                .copied(),
        }
    }

    /// A bearer token for API calls: the scope's installation token, minted
    /// when missing or within two minutes of expiry.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when the installation cannot be found or
    /// the token request fails.
    pub async fn bearer(
        &self,
        http: &reqwest::Client,
        api_base: &str,
        retry: RetryPolicy,
    ) -> Result<SecretString, PlatformError> {
        match self {
            Self::Token(token) => Ok(token.clone()),
            Self::App { installations, .. } => {
                let id = self.installation(http, api_base, retry).await?;
                installations.token(http, api_base, id, retry).await
            }
        }
    }
}

async fn mint_installation_token(
    http: &reqwest::Client,
    api_base: &str,
    credentials: &AppCredentials,
    installation: u64,
    now: SystemTime,
) -> Result<CachedToken, PlatformError> {
    let jwt = credentials.jwt(now)?;
    let url = format!("{api_base}/app/installations/{installation}/access_tokens");
    let response = http
        .post(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(PlatformError::status(
            status.as_u16(),
            &body,
            Some(&headers),
        ));
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| PlatformError::Decode(e.to_string()))?;
    let token = value.get("token").and_then(Value::as_str).ok_or_else(|| {
        PlatformError::Decode("installation token response without token".to_owned())
    })?;
    debug!(installation, "minted installation token");
    Ok(CachedToken {
        token: SecretString::from(token.to_owned()),
        expires_at: expiry(&value, now),
    })
}

/// When a token expires: the response's `expires_at`, or 55 minutes from
/// now when it has none GitHub's way (tokens last an hour).
fn expiry(response: &Value, now: SystemTime) -> SystemTime {
    response
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
        .map_or_else(|| now + Duration::from_mins(55), SystemTime::from)
}

impl CachedToken {
    /// For tests: the token value.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.token.expose_secret()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use serde_json::json;

    use super::expiry;

    #[test]
    fn a_token_expires_when_github_says() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let at = expiry(&json!({"expires_at": "2023-11-14T22:23:20Z"}), now);
        assert_eq!(at, UNIX_EPOCH + Duration::from_secs(1_700_000_600));
        assert_eq!(
            expiry(&json!({"expires_at": "soon"}), now),
            now + Duration::from_mins(55),
            "an unreadable expiry falls back to 55 minutes"
        );
    }
}
