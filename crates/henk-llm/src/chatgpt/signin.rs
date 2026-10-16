//! The browser sign-in (#620): OpenAI's authorization code flow with PKCE,
//! for an app that runs on the user's own machine.
//!
//! [`SignIn::start`] opens a listener on `127.0.0.1` and returns a
//! [`Pending`] sign-in with the URL to open in a browser; [`Pending::finish`]
//! waits for the browser to come back to the listener, exchanges the code,
//! checks the ID token and returns the [`Credentials`] to save. The first
//! sign-in registers Henk with OpenAI (`dynamic_agent_client`), which
//! returns a client id bound to the user and workspace; a later one reuses
//! the client id and host id of the existing file.
//!
//! A code, a verifier or a token is never in an error.

use std::fmt::{self, Write as _};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest;
use ring::rand::{SecureRandom as _, SystemRandom};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use super::credentials::Credentials;
use super::idtoken::{self, Expected};
use super::manager::{Clock, TOKEN_URL, error_code, form_encoded, system_clock};
use crate::error::truncate_body;

/// Where the browser is sent.
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
/// OpenAI's published signing keys.
pub const JWKS_URL: &str = "https://auth.openai.com/.well-known/jwks.json";
/// The issuer of the ID token.
pub const ISSUER: &str = "https://auth.openai.com";
/// The port OpenAI's docs use for the callback.
pub const DEFAULT_PORT: u16 = 1455;

/// What the sign-in asks to use: the Responses API of OpenAI.
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Without this scope the tokens do not pay from the plan.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// The client id of a first sign-in, before OpenAI has issued one.
const REGISTRATION_CLIENT: &str = "dynamic_agent_client";
/// The name shown where the user authorizes the agent.
const AGENT_NAME: &str = "Henk";
/// The path of the callback; only the port may differ between sign-ins.
const CALLBACK_PATH: &str = "/auth/callback";
/// How long one connection to the listener may take to send its request.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause after a failed accept, so a persistent error is not a busy loop.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// How long OpenAI may take to answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a sign-in did not complete. None of them quotes a token or a code.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    /// The callback port cannot be listened on.
    #[error("cannot listen on 127.0.0.1:{port}: {source}; pass another port with --port")]
    Listen {
        /// The port.
        port: u16,
        /// Why not.
        #[source]
        source: std::io::Error,
    },
    /// The random values of the sign-in cannot be had.
    #[error("the system's random source is unavailable")]
    Random,
    /// The browser did not come back in time.
    #[error("no sign-in arrived within {0} seconds")]
    TimedOut(u64),
    /// The user said no, or OpenAI refused, in the browser.
    #[error("the sign-in was not completed in the browser ({0})")]
    Denied(String),
    /// The callback came back without a code.
    #[error("the callback carried no authorization code")]
    NoCode,
    /// A first sign-in came back without the client id OpenAI issues.
    #[error("OpenAI did not issue a client id for this sign-in, so the registration is incomplete")]
    NoClientId,
    /// A repeated sign-in came back naming another client.
    #[error("OpenAI named another client than the one this sign-in file was issued to")]
    ClientChanged,
    /// The token endpoint did not accept the code.
    #[error("OpenAI refused the sign-in ({0})")]
    Refused(String),
    /// The token endpoint or the key set could not be reached.
    #[error("could not reach OpenAI: {0}")]
    Network(String),
    /// The token endpoint answered with something that is not a token.
    #[error("OpenAI answered with something unreadable")]
    Unreadable,
    /// The sign-in returned no refresh token, so it could not be renewed.
    #[error("OpenAI returned no refresh token; without one the sign-in could not be renewed")]
    NoRefreshToken,
    /// The ID token is not acceptable.
    #[error("the ID token is not acceptable: {0}")]
    IdToken(String),
    /// The plan scope was not granted.
    #[error(
        "the sign-in did not grant {PLAN_SCOPE}, which paying from a ChatGPT plan needs; granted: {0}"
    )]
    MissingScope(String),
    /// The file is for another account.
    #[error(
        "this sign-in is for {now}, but the sign-in file belongs to {was}; remove the file to sign in as someone else"
    )]
    OtherAccount {
        /// The account in the file.
        was: String,
        /// The account that just signed in.
        now: String,
    },
}

/// The sign-in flow against OpenAI, or against a stand-in in tests.
#[derive(Clone)]
pub struct SignIn {
    authorize_url: String,
    token_url: String,
    jwks_url: String,
    http: reqwest::Client,
    clock: Clock,
}

impl fmt::Debug for SignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignIn")
            .field("token_url", &self.token_url)
            .finish_non_exhaustive()
    }
}

impl SignIn {
    /// The flow against OpenAI over `http`.
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
            jwks_url: JWKS_URL.to_owned(),
            http,
            clock: system_clock(),
        }
    }

    /// The flow against other URLs: the page the browser is sent to, the
    /// token endpoint and the key set. For tests.
    #[must_use]
    pub fn with_urls(
        mut self,
        authorize: impl Into<String>,
        token: impl Into<String>,
        jwks: impl Into<String>,
    ) -> Self {
        self.authorize_url = authorize.into();
        self.token_url = token.into();
        self.jwks_url = jwks.into();
        self
    }

    /// The flow with its own clock. For tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Starts a sign-in: listens on `127.0.0.1:port` (0 for any free port)
    /// and builds the URL for the browser.
    ///
    /// `existing` is the sign-in file being replaced, if there is one. Its
    /// client id and host id are reused, and the account that signs in must
    /// be the same.
    ///
    /// # Errors
    ///
    /// Returns [`SignInError::Listen`] when the port is taken and
    /// [`SignInError::Random`] when no randomness is available.
    pub async fn start(
        &self,
        existing: Option<Credentials>,
        port: u16,
    ) -> Result<Pending, SignInError> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|source| SignInError::Listen { port, source })?;
        let port = listener
            .local_addr()
            .map_err(|source| SignInError::Listen { port, source })?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

        let state = random_text(32)?;
        let nonce = random_text(32)?;
        let verifier = random_text(32)?;
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
        let host_id = match &existing {
            Some(existing) => existing.ext_agent_host_id.clone(),
            None => host_id()?,
        };
        let client_id = existing
            .as_ref()
            .map_or(REGISTRATION_CLIENT, |existing| existing.client_id.as_str());

        let mut query: Vec<(&str, &str)> = vec![
            ("client_id", client_id),
            ("ext_agent_host_id", &host_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", &state),
            ("nonce", &nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ];
        // The URL is printed and handed to the browser opener, so no token
        // goes into it. The account's email is the only hint.
        match &existing {
            None => query.push(("agent_name_hint", AGENT_NAME)),
            Some(existing) => {
                if let Some(account) = existing
                    .account
                    .as_deref()
                    .filter(|account| account.contains('@'))
                {
                    query.push(("login_hint", account));
                }
            }
        }
        let authorization_url = reqwest::Url::parse_with_params(&self.authorize_url, &query)
            .map_err(|_| SignInError::Random)?
            .to_string();

        Ok(Pending {
            flow: self.clone(),
            listener,
            redirect_uri,
            authorization_url,
            state,
            nonce,
            verifier: verifier.into(),
            host_id,
            existing,
        })
    }
}

/// A sign-in that has started: the listener is up and the browser has yet
/// to come back.
pub struct Pending {
    flow: SignIn,
    listener: TcpListener,
    redirect_uri: String,
    authorization_url: String,
    state: String,
    nonce: String,
    verifier: SecretString,
    host_id: String,
    existing: Option<Credentials>,
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pending")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// What the browser brought back.
struct Callback {
    code: Option<SecretString>,
    error: Option<String>,
    client_id: Option<String>,
    scope: Option<String>,
}

impl Pending {
    /// The URL to open in a browser. It carries a one-time state and a
    /// PKCE challenge, and is of no use to anyone else.
    #[must_use]
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// The address the browser comes back to.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits up to `wait` for the browser, then completes the sign-in.
    ///
    /// # Errors
    ///
    /// Returns why the sign-in did not complete.
    pub async fn finish(self, wait: Duration) -> Result<Credentials, SignInError> {
        let callback = tokio::time::timeout(wait, self.wait_for_callback())
            .await
            .map_err(|_| SignInError::TimedOut(wait.as_secs()))?;
        if let Some(error) = &callback.error {
            return Err(SignInError::Denied(error.clone()));
        }
        let code = callback.code.as_ref().ok_or(SignInError::NoCode)?;
        let client_id = match (&self.existing, &callback.client_id) {
            (None, Some(issued)) if !issued.is_empty() => issued.clone(),
            (None, _) => return Err(SignInError::NoClientId),
            (Some(existing), Some(named)) if *named != existing.client_id => {
                return Err(SignInError::ClientChanged);
            }
            (Some(existing), _) => existing.client_id.clone(),
        };

        let tokens = self.exchange(code, &client_id).await?;
        let now = (self.flow.clock)();
        let granted: Vec<String> = tokens
            .scope
            .as_deref()
            .or(callback.scope.as_deref())
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if !granted.iter().any(|scope| scope == PLAN_SCOPE) {
            return Err(SignInError::MissingScope(granted.join(" ")));
        }
        let refresh_token = tokens
            .refresh_token
            .filter(|token| !token.is_empty())
            .ok_or(SignInError::NoRefreshToken)?;
        let id_token = tokens
            .id_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| SignInError::IdToken("OpenAI returned none".to_owned()))?;
        let jwks = self.keys().await?;
        let identity = idtoken::verify(
            &id_token,
            &jwks,
            &Expected {
                issuer: ISSUER,
                audience: &client_id,
                nonce: &self.nonce,
                now,
            },
        )
        .map_err(SignInError::IdToken)?;
        let account = identity.email.unwrap_or(identity.subject);
        if let Some(was) = self
            .existing
            .as_ref()
            .and_then(|existing| existing.account.as_deref())
            .filter(|was| *was != account)
        {
            return Err(SignInError::OtherAccount {
                was: was.to_owned(),
                now: account,
            });
        }

        Ok(Credentials {
            client_id,
            ext_agent_host_id: self.host_id,
            access_token: tokens.access_token.into(),
            refresh_token: refresh_token.into(),
            id_token: Some(id_token.into()),
            expires_at: now.saturating_add(tokens.expires_in.unwrap_or(3600)),
            earliest_refresh_at: tokens.earliest_refresh_at.as_ref().and_then(Value::as_i64),
            refreshed_at: now,
            scopes: granted,
            account: Some(account),
        })
    }

    /// Serves the listener until the browser comes back with this
    /// sign-in's state.
    async fn wait_for_callback(&self) -> Callback {
        loop {
            let (mut stream, _) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    // A persistent error such as EMFILE returns at once;
                    // pause so the wait is not a busy loop.
                    warn!("the sign-in listener could not accept a connection: {error}");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                    continue;
                }
            };
            let Some(request) = tokio::time::timeout(CONNECTION_TIMEOUT, read_request(&mut stream))
                .await
                .ok()
                .flatten()
            else {
                respond(&mut stream, "400 Bad Request", PAGE_NOT_FOR_HENK).await;
                continue;
            };
            match self.callback_of(&request) {
                Ok(callback) => {
                    respond(&mut stream, "200 OK", PAGE_DONE).await;
                    return callback;
                }
                Err(page) => {
                    debug!("a request to the sign-in listener was not this sign-in's callback");
                    respond(&mut stream, page.0, page.1).await;
                }
            }
        }
    }

    /// The callback in `request`, or the page to answer with when it is
    /// not this sign-in's.
    fn callback_of(&self, request: &Request) -> Result<Callback, (&'static str, &'static str)> {
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{}", request.target))
            .map_err(|_| ("400 Bad Request", PAGE_NOT_FOR_HENK))?;
        if url.path() != CALLBACK_PATH {
            return Err(("404 Not Found", PAGE_NOT_FOR_HENK));
        }
        let expected_host = self
            .redirect_uri
            .strip_prefix("http://")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");
        if request.host.as_deref() != Some(expected_host) {
            return Err(("400 Bad Request", PAGE_NOT_FOR_HENK));
        }
        let mut callback = Callback {
            code: None,
            error: None,
            client_id: None,
            scope: None,
        };
        let mut state = None;
        for (name, value) in url.query_pairs() {
            match name.as_ref() {
                "state" => state = Some(value.into_owned()),
                "code" => callback.code = Some(value.into_owned().into()),
                "error" => callback.error = Some(word(&value)),
                "client_id" => callback.client_id = Some(value.into_owned()),
                "scope" => callback.scope = Some(value.into_owned()),
                _ => {}
            }
        }
        if state.as_deref() != Some(self.state.as_str()) {
            return Err(("400 Bad Request", PAGE_NOT_FOR_HENK));
        }
        Ok(callback)
    }

    /// Trades the code for tokens.
    async fn exchange(&self, code: &SecretString, client_id: &str) -> Result<Tokens, SignInError> {
        let body = format!(
            "grant_type=authorization_code&client_id={}&code={}&code_verifier={}&redirect_uri={}&resource={}",
            form_encoded(client_id),
            form_encoded(code.expose_secret()),
            form_encoded(self.verifier.expose_secret()),
            form_encoded(&self.redirect_uri),
            form_encoded(RESOURCE),
        );
        let response = self
            .flow
            .http
            .post(&self.flow.token_url)
            .timeout(REQUEST_TIMEOUT)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(network)?;
        let status = response.status();
        let text = response.text().await.map_err(network)?;
        if !status.is_success() {
            let why = error_code(&text).map_or_else(
                || format!("HTTP {}", status.as_u16()),
                |code| format!("HTTP {}, {code}", status.as_u16()),
            );
            warn!(status = status.as_u16(), "the sign-in was refused");
            return Err(SignInError::Refused(why));
        }
        serde_json::from_str(&text).map_err(|_| SignInError::Unreadable)
    }

    /// OpenAI's published signing keys.
    async fn keys(&self) -> Result<Value, SignInError> {
        let response = self
            .flow
            .http
            .get(&self.flow.jwks_url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(network)?;
        if !response.status().is_success() {
            return Err(SignInError::Network(format!(
                "the key set answered HTTP {}",
                response.status().as_u16()
            )));
        }
        let text = response.text().await.map_err(network)?;
        serde_json::from_str(&text).map_err(|_| {
            SignInError::Network(format!(
                "the key set is not JSON: {}",
                truncate_body(&text).chars().take(80).collect::<String>()
            ))
        })
    }
}

/// The token endpoint's answer to a code.
#[derive(Deserialize)]
struct Tokens {
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

fn network(error: reqwest::Error) -> SignInError {
    SignInError::Network(error.without_url().to_string())
}

/// `value` cut to the characters of an OAuth error code.
fn word(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect()
}

/// `bytes` random bytes as base64url text.
fn random_text(bytes: usize) -> Result<String, SignInError> {
    let mut buffer = vec![0_u8; bytes];
    SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| SignInError::Random)?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// A fresh host id: a random (version 4) UUID as a URN.
fn host_id() -> Result<String, SignInError> {
    let mut bytes = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| SignInError::Random)?;
    if let Some(version) = bytes.get_mut(6) {
        *version = (*version & 0x0f) | 0x40;
    }
    if let Some(variant) = bytes.get_mut(8) {
        *variant = (*variant & 0x3f) | 0x80;
    }
    let mut hex = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        hex.get(..8).unwrap_or_default(),
        hex.get(8..12).unwrap_or_default(),
        hex.get(12..16).unwrap_or_default(),
        hex.get(16..20).unwrap_or_default(),
        hex.get(20..).unwrap_or_default(),
    ))
}

/// A request line and its `Host` header.
struct Request {
    target: String,
    host: Option<String>,
}

/// Reads one request up to the end of its headers.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        if buffer.len() > 16 * 1024 {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(chunk.get(..read)?);
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    if first.next()? != "GET" {
        return None;
    }
    let target = first.next()?.to_owned();
    let host = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("host")
            .then(|| value.trim().to_owned())
    });
    Some(Request { target, host })
}

const PAGE_DONE: &str = "<!doctype html><meta charset=\"utf-8\"><title>Henk</title><p>Signed in. You can close this tab and go back to the terminal.</p>";
const PAGE_NOT_FOR_HENK: &str = "<!doctype html><meta charset=\"utf-8\"><title>Henk</title><p>This is not the sign-in Henk is waiting for.</p>";

async fn respond(stream: &mut TcpStream, status: &str, page: &str) {
    let answer = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(answer.as_bytes()).await;
    let _ = stream.shutdown().await;
}
