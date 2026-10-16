//! The token manager: renews a ChatGPT sign-in and hands out its bearer.

use std::fmt::{self, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::credentials::{Credentials, write_atomic};
use crate::client::TokenSource;
use crate::error::{LlmError, truncate_body};

/// OpenAI's token endpoint for ChatGPT sign-ins.
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";

/// Renew when the access token has this many seconds left or fewer.
const RENEW_BEFORE: i64 = 300;

/// How long a renewal may take.
const RENEWAL_TIMEOUT: Duration = Duration::from_secs(30);

/// What OpenAI calls a refresh token that will not renew again.
const GONE: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// The time, in seconds since the epoch. Tests set their own.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
    })
}

/// Keeps one endpoint's ChatGPT sign-in valid. See the module documentation
/// for what it promises.
pub struct TokenManager {
    endpoint: String,
    path: PathBuf,
    token_url: String,
    http: reqwest::Client,
    clock: Clock,
    state: Mutex<State>,
}

impl fmt::Debug for TokenManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenManager")
            .field("endpoint", &self.endpoint)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    credentials: Option<Credentials>,
    /// What the file looked like when it was last read or written.
    stamp: Option<Stamp>,
    /// Why the endpoint is signed out, until the file changes.
    signed_out: Option<String>,
    /// Tokens from a renewal that are not on disk yet. Nothing is handed
    /// out until they are.
    unsaved: bool,
    /// A request was refused with the current token: renew at the next call.
    force: bool,
}

/// Enough of a file's metadata to tell that it was replaced.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    inode: u64,
    modified: i64,
    modified_nanos: i64,
    len: u64,
}

impl TokenManager {
    /// A manager for `endpoint` (named in its errors) over the credential
    /// file at `path`, renewing through OpenAI's token endpoint with `http`.
    #[must_use]
    pub fn new(
        endpoint: impl Into<String>,
        path: impl Into<PathBuf>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            path: path.into(),
            token_url: TOKEN_URL.to_owned(),
            http,
            clock: system_clock(),
            state: Mutex::new(State::default()),
        }
    }

    /// The same manager renewing through `url`, for a stub in tests.
    #[must_use]
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// The same manager reading time from `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn signed_out(&self, reason: &str) -> LlmError {
        LlmError::SignedOut {
            endpoint: self.endpoint.clone(),
            reason: reason.to_owned(),
        }
    }

    fn file_problem(&self, problem: impl Into<String>) -> LlmError {
        LlmError::CredentialFile {
            endpoint: self.endpoint.clone(),
            problem: problem.into(),
        }
    }

    /// Takes the file's credentials when it changed since they were read.
    async fn reload(&self, state: &mut State) -> Result<(), LlmError> {
        let metadata = match tokio::fs::metadata(&self.path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if state.credentials.is_some() {
                    // Keep going from memory, and write the file again.
                    state.unsaved = true;
                    state.stamp = None;
                }
                return Ok(());
            }
            Err(error) => {
                return Err(self.file_problem(format!("{}: {error}", self.path.display())));
            }
        };
        let stamp = Stamp {
            inode: metadata.ino(),
            modified: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
            len: metadata.len(),
        };
        if state.stamp == Some(stamp) {
            return Ok(());
        }
        let text = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|error| self.file_problem(format!("{}: {error}", self.path.display())))?;
        let credentials = Credentials::from_json(&text)
            .map_err(|why| self.file_problem(format!("{}: {why}", self.path.display())))?;
        debug!(endpoint = %self.endpoint, "read the sign-in file");
        state.credentials = Some(credentials);
        state.stamp = Some(stamp);
        state.signed_out = None;
        state.unsaved = false;
        state.force = false;
        Ok(())
    }

    /// Writes the credentials to the file.
    async fn save(&self, state: &mut State) -> Result<(), LlmError> {
        let Some(credentials) = state.credentials.as_ref() else {
            return Ok(());
        };
        let content = credentials
            .to_json()
            .map_err(|why| self.file_problem(why))?;
        let path = self.path.clone();
        let written = tokio::task::spawn_blocking(move || write_atomic(&path, &content))
            .await
            .map_err(|error| self.file_problem(error.to_string()))?;
        written.map_err(|error| {
            self.file_problem(format!(
                "{}: {error}; the renewed sign-in is kept in memory and written at the next call",
                self.path.display()
            ))
        })?;
        state.unsaved = false;
        if let Ok(metadata) = tokio::fs::metadata(&self.path).await {
            state.stamp = Some(Stamp {
                inode: metadata.ino(),
                modified: metadata.mtime(),
                modified_nanos: metadata.mtime_nsec(),
                len: metadata.len(),
            });
        }
        Ok(())
    }

    /// Asks OpenAI for a new token pair.
    async fn renew(&self, current: &Credentials) -> Result<Credentials, LlmError> {
        let body = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            form_encoded(current.refresh_token.expose_secret()),
            form_encoded(&current.client_id),
        );
        let response = self
            .http
            .post(&self.token_url)
            .timeout(RENEWAL_TIMEOUT)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(LlmError::Transport)?;
        let status = response.status();
        let text = response.text().await.map_err(LlmError::Transport)?;
        if !status.is_success() {
            return Err(self.renewal_refused(status.as_u16(), &text, current));
        }
        let renewed: Renewed = serde_json::from_str(&text).map_err(|_| {
            LlmError::Decode("the token endpoint answered with something unreadable".to_owned())
        })?;
        let now = self.now();
        let mut next = current.clone();
        next.access_token = renewed.access_token.into();
        if let Some(refresh) = renewed.refresh_token.filter(|t| !t.is_empty()) {
            next.refresh_token = refresh.into();
        }
        if let Some(id_token) = renewed.id_token.filter(|t| !t.is_empty()) {
            next.id_token = Some(id_token.into());
        }
        next.expires_at = now.saturating_add(renewed.expires_in.unwrap_or(3600));
        next.earliest_refresh_at = renewed.earliest_refresh_at.as_ref().and_then(Value::as_i64);
        next.refreshed_at = now;
        if let Some(scope) = renewed.scope.filter(|s| !s.is_empty()) {
            next.scopes = scope.split_whitespace().map(str::to_owned).collect();
        }
        Ok(next)
    }

    /// The error for a renewal OpenAI did not accept.
    fn renewal_refused(&self, status: u16, body: &str, current: &Credentials) -> LlmError {
        if let Some(code) = error_code(body)
            && GONE.contains(&code.as_str())
        {
            return self.signed_out(&code);
        }
        let body = truncate_body(&scrub(body, current));
        match status {
            429 => LlmError::RateLimited { retry_after: None },
            408 | 500..=599 => LlmError::Server { status, body },
            _ => LlmError::BadRequest { status, body },
        }
    }
}

/// The renewal answer.
#[derive(Deserialize)]
struct Renewed {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    earliest_refresh_at: Option<Value>,
}

#[async_trait::async_trait]
impl TokenSource for TokenManager {
    async fn bearer(&self) -> Result<SecretString, LlmError> {
        // The lock is held through a renewal: callers that arrive meanwhile
        // wait, then find the new token and take it.
        let mut state = self.state.lock().await;
        self.reload(&mut state).await?;
        if let Some(reason) = state.signed_out.clone() {
            return Err(self.signed_out(&reason));
        }
        if state.credentials.is_none() {
            return Err(self.signed_out("no sign-in file yet"));
        }
        if state.unsaved {
            self.save(&mut state).await?;
        }
        let Some(current) = state.credentials.clone() else {
            return Err(self.signed_out("no sign-in file yet"));
        };
        if !state.force && self.now().saturating_add(RENEW_BEFORE) < current.expires_at {
            return Ok(current.access_token);
        }
        debug!(endpoint = %self.endpoint, "renewing the ChatGPT sign-in");
        match self.renew(&current).await {
            Ok(next) => {
                state.credentials = Some(next.clone());
                state.force = false;
                state.unsaved = true;
                // Saved before it is used: the replacement refresh token
                // may be the only one that works from here on.
                self.save(&mut state).await?;
                info!(endpoint = %self.endpoint, "renewed the ChatGPT sign-in");
                Ok(next.access_token)
            }
            Err(error @ LlmError::SignedOut { .. }) => {
                if let LlmError::SignedOut { reason, .. } = &error {
                    warn!(endpoint = %self.endpoint, reason = %reason, "the ChatGPT sign-in cannot be renewed");
                    state.signed_out = Some(reason.clone());
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    async fn rejected(&self, token: &SecretString) {
        let mut state = self.state.lock().await;
        if state
            .credentials
            .as_ref()
            .is_some_and(|c| c.access_token.expose_secret() == token.expose_secret())
        {
            state.force = true;
        }
    }
}

impl fmt::Debug for dyn TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenSource")
    }
}

/// The `error` of an OAuth error body, as a short word: the string itself
/// or the `code` inside an object. Only letters, digits and underscores are
/// kept, so nothing the server sends is echoed.
fn error_code(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let code = error
        .as_str()
        .or_else(|| error.get("code").and_then(Value::as_str))?;
    let code: String = code
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
        .take(64)
        .collect();
    (!code.is_empty()).then_some(code)
}

/// `body` without any token of `credentials`.
fn scrub(body: &str, credentials: &Credentials) -> String {
    let mut out = body.to_owned();
    for secret in [
        Some(credentials.access_token.expose_secret()),
        Some(credentials.refresh_token.expose_secret()),
        credentials
            .id_token
            .as_ref()
            .map(secrecy::ExposeSecret::expose_secret),
    ]
    .into_iter()
    .flatten()
    {
        if !secret.is_empty() {
            out = out.replace(secret, "[removed]");
        }
    }
    out
}

/// `value` as `application/x-www-form-urlencoded` text.
fn form_encoded(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_text_keeps_the_unreserved_and_encodes_the_rest() {
        assert_eq!(form_encoded("aZ09-._~"), "aZ09-._~");
        assert_eq!(form_encoded("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }

    #[test]
    fn an_error_code_is_a_word_from_a_string_or_an_object() {
        assert_eq!(
            error_code(r#"{"error":"invalid_grant","error_description":"x"}"#).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            error_code(r#"{"error":{"code":"refresh_token_reused","message":"m"}}"#).as_deref(),
            Some("refresh_token_reused")
        );
        assert_eq!(error_code("not json"), None);
        assert_eq!(
            error_code(r#"{"error":"Bad <script> Thing!"}"#).as_deref(),
            Some("adscripthing"),
            "only letters, digits and underscores survive"
        );
    }
}
