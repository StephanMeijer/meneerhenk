//! GitHub App authentication without a JWT crate: RS256 on ring.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use tracing::debug;

use crate::error::PlatformError;

/// The App's identity and private key.
pub struct AppCredentials {
    app_id: u64,
    installation_id: u64,
    key: RsaKeyPair,
}

impl std::fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppCredentials")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
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
    pub fn from_pem(app_id: u64, installation_id: u64, pem: &str) -> Result<Self, PlatformError> {
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
        Ok(Self {
            app_id,
            installation_id,
            key,
        })
    }

    /// The installation this process acts for.
    #[must_use]
    pub const fn installation_id(&self) -> u64 {
        self.installation_id
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

/// How the client authenticates to GitHub.
pub enum GitHubAuth {
    /// A GitHub App: installation tokens minted from the App key and cached.
    App {
        /// The credentials.
        credentials: Box<AppCredentials>,
        /// The cached installation token.
        cache: Mutex<Option<CachedToken>>,
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
            Self::App { credentials, .. } => f.debug_tuple("App").field(credentials).finish(),
            Self::Token(_) => f.write_str("Token(..)"),
        }
    }
}

impl GitHubAuth {
    /// App authentication.
    #[must_use]
    pub fn app(credentials: AppCredentials) -> Self {
        Self::App {
            credentials: Box::new(credentials),
            cache: Mutex::new(None),
        }
    }

    /// A fixed token.
    #[must_use]
    pub fn token(token: SecretString) -> Self {
        Self::Token(token)
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
            Self::App { credentials, .. } => {
                Some(credentials.jwt(SystemTime::now()).map(SecretString::from))
            }
        }
    }

    /// A bearer token for API calls, minting an installation token when the
    /// cached one is missing or within two minutes of expiry.
    ///
    /// # Errors
    ///
    /// Returns [`PlatformError`] when the token request fails.
    pub async fn bearer(
        &self,
        http: &reqwest::Client,
        api_base: &str,
    ) -> Result<SecretString, PlatformError> {
        match self {
            Self::Token(token) => Ok(token.clone()),
            Self::App { credentials, cache } => {
                let now = SystemTime::now();
                if let Ok(cache) = cache.lock()
                    && let Some(cached) = cache.as_ref()
                    && cached.expires_at > now + Duration::from_mins(2)
                {
                    return Ok(cached.token.clone());
                }
                let fresh = mint_installation_token(http, api_base, credentials, now).await?;
                if let Ok(mut cache) = cache.lock() {
                    *cache = Some(fresh.clone());
                }
                Ok(fresh.token)
            }
        }
    }
}

async fn mint_installation_token(
    http: &reqwest::Client,
    api_base: &str,
    credentials: &AppCredentials,
    now: SystemTime,
) -> Result<CachedToken, PlatformError> {
    let jwt = credentials.jwt(now)?;
    let url = format!(
        "{api_base}/app/installations/{}/access_tokens",
        credentials.installation_id
    );
    let response = http
        .post(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(PlatformError::Status {
            status: status.as_u16(),
            body: crate::error::truncate(&body),
        });
    }
    let value: Value =
        serde_json::from_str(&body).map_err(|e| PlatformError::Decode(e.to_string()))?;
    let token = value.get("token").and_then(Value::as_str).ok_or_else(|| {
        PlatformError::Decode("installation token response without token".to_owned())
    })?;
    // GitHub says tokens last an hour; a conservative fixed lifetime avoids
    // parsing the timestamp in the body.
    debug!(
        installation = credentials.installation_id,
        "minted installation token"
    );
    Ok(CachedToken {
        token: SecretString::from(token.to_owned()),
        expires_at: now + Duration::from_mins(55),
    })
}

impl CachedToken {
    /// For tests: the token value.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.token.expose_secret()
    }
}
