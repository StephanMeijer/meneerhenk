//! Sign-in with GitHub. GitHub tells Henk who someone is; the configured
//! list of GitHub user ids decides whether they may look (§2). The GitHub
//! token is used once, to read the account, and then dropped.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ring::rand::{SecureRandom as _, SystemRandom};
use secrecy::ExposeSecret as _;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;
use tracing::warn;

use super::Dashboard;
use super::session::{SESSION_COOKIE, STATE_COOKIE, Session, cookie_value};
use crate::pages::page;

/// Someone signed in and allowed. Every dashboard page takes one.
#[derive(Debug, Clone)]
pub struct Viewer(pub Session);

impl FromRequestParts<Arc<Dashboard>> for Viewer {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        dashboard: &Arc<Dashboard>,
    ) -> Result<Self, Self::Rejection> {
        let session = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|cookies| cookie_value(cookies, SESSION_COOKIE))
            .and_then(|value| dashboard.signer.read_session(value))
            // Checked on every request, so taking an id off the list ends
            // its access at once.
            .filter(|s| dashboard.config.allowed_github_ids.contains(&s.github_id));
        match session {
            Some(session) => Ok(Self(session)),
            None if parts.uri.path() == "/dashboard/running.json" => {
                Err((StatusCode::UNAUTHORIZED, "sign in first").into_response())
            }
            None => {
                // Back to this page after sign-in, so a run link posted on
                // a pull request lands on that run (#69).
                let here = parts
                    .uri
                    .path_and_query()
                    .map_or("/dashboard", |p| p.as_str());
                let to = match safe_next(here).filter(|next| *next != "/dashboard") {
                    Some(next) => format!("/dashboard/login?next={}", encode(next)),
                    None => "/dashboard/login".to_owned(),
                };
                Err(redirect(&to, None))
            }
        }
    }
}

/// `next` when it is a dashboard path this server can safely send a
/// browser to after sign-in: under `/dashboard`, printable ASCII, no
/// backslash, short. Anything else is dropped, so sign-in can never
/// redirect off the site.
pub(super) fn safe_next(next: &str) -> Option<&str> {
    let under =
        next == "/dashboard" || next.starts_with("/dashboard/") || next.starts_with("/dashboard?");
    let printable = next.bytes().all(|b| b.is_ascii_graphic() && b != b'\\');
    (under && printable && next.len() <= 512).then_some(next)
}

/// What `/dashboard/login` takes.
#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    next: Option<String>,
}

fn redirect(to: &str, cookie: Option<String>) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = HeaderValue::from_str(to) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    if let Some(cookie) = cookie
        && let Ok(value) = HeaderValue::from_str(&cookie)
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// Percent-encodes a query value.
pub(super) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn notice(status: StatusCode, title: &str, text: &str) -> Response {
    let body = format!(
        "<h1>{}</h1><p>{}</p>",
        crate::pages::escape(title),
        crate::pages::escape(text)
    );
    (status, page(title, "", &body)).into_response()
}

/// Sends the browser to GitHub with a fresh state, kept in a signed cookie
/// together with the page to return to.
pub async fn login(
    State(dashboard): State<Arc<Dashboard>>,
    Query(query): Query<LoginQuery>,
) -> Response {
    let mut bytes = [0_u8; 24];
    if SystemRandom::new().fill(&mut bytes).is_err() {
        return notice(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Sign-in failed",
            "No randomness.",
        );
    }
    let state: String = bytes.iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    });
    let to = format!(
        "{}/login/oauth/authorize?client_id={}&redirect_uri={}&state={state}&allow_signup=false",
        dashboard.config.github_web_base.trim_end_matches('/'),
        encode(&dashboard.client_id),
        encode(&dashboard.redirect_uri()),
    );
    let next = query.next.as_deref().and_then(safe_next).unwrap_or("");
    let cookie = dashboard.signer.cookie(
        STATE_COOKIE,
        &dashboard.signer.state(&format!("{state}|{next}")),
        Duration::from_mins(10),
    );
    redirect(&to, Some(cookie))
}

/// What GitHub sends back.
#[derive(Debug, Deserialize)]
pub struct Callback {
    code: Option<String>,
    state: Option<String>,
}

/// Finishes the sign-in: checks the state, asks GitHub who this is, and
/// lets them in only when their id is on the list.
pub async fn callback(
    State(dashboard): State<Arc<Dashboard>>,
    headers: HeaderMap,
    Query(callback): Query<Callback>,
) -> Response {
    let expected = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| cookie_value(cookies, STATE_COOKIE))
        .and_then(|value| dashboard.signer.read_state(value));
    let (expected, next) = match expected.as_deref().and_then(|v| v.split_once('|')) {
        Some((state, next)) => (Some(state.to_owned()), safe_next(next).map(str::to_owned)),
        None => (None, None),
    };
    let (Some(code), Some(state), Some(expected)) = (callback.code, callback.state, expected)
    else {
        return notice(
            StatusCode::BAD_REQUEST,
            "Sign-in failed",
            "The sign-in expired or did not start here. Try again.",
        );
    };
    if !bool::from(state.as_bytes().ct_eq(expected.as_bytes())) {
        return notice(
            StatusCode::BAD_REQUEST,
            "Sign-in failed",
            "The sign-in did not start here. Try again.",
        );
    }
    let user = match github_user(&dashboard, &code).await {
        Ok(user) => user,
        Err(error) => {
            warn!(%error, "GitHub sign-in failed");
            return notice(
                StatusCode::BAD_GATEWAY,
                "Sign-in failed",
                "GitHub did not confirm who you are. Try again.",
            );
        }
    };
    let clear_state = dashboard.signer.clear(STATE_COOKIE);
    if !dashboard
        .config
        .allowed_github_ids
        .contains(&user.github_id)
    {
        warn!(
            github_id = user.github_id,
            "a GitHub account not on the list tried to sign in"
        );
        let mut response = notice(
            StatusCode::FORBIDDEN,
            "Not for you",
            "This GitHub account may not see Henk's dashboard.",
        );
        if let Ok(value) = HeaderValue::from_str(&clear_state) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
        return response;
    }
    let ttl = Duration::from_hours(u64::from(dashboard.config.session_hours));
    let session =
        dashboard
            .signer
            .cookie(SESSION_COOKIE, &dashboard.signer.session(&user, ttl), ttl);
    let mut response = redirect(next.as_deref().unwrap_or("/dashboard"), Some(session));
    if let Ok(value) = HeaderValue::from_str(&clear_state) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// Exchanges the code for a token and reads the account it belongs to.
async fn github_user(dashboard: &Dashboard, code: &str) -> anyhow::Result<Session> {
    let token: Value = dashboard
        .http
        .post(format!(
            "{}/login/oauth/access_token",
            dashboard.config.github_web_base.trim_end_matches('/')
        ))
        .header(header::ACCEPT, "application/json")
        .json(&json!({
            "client_id": dashboard.client_id,
            "client_secret": dashboard.client_secret.expose_secret(),
            "code": code,
            "redirect_uri": dashboard.redirect_uri(),
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("no access token in GitHub's answer"))?;
    let user: Value = dashboard
        .http
        .get(format!(
            "{}/user",
            dashboard.config.github_api_base.trim_end_matches('/')
        ))
        .bearer_auth(access)
        .header(header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(Session {
        github_id: user
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("GitHub user without id"))?,
        login: user
            .get("login")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    })
}

/// Signs out.
pub async fn logout(State(dashboard): State<Arc<Dashboard>>) -> Response {
    let mut response = notice(
        StatusCode::OK,
        "Signed out",
        "You are signed out of Henk's dashboard.",
    );
    if let Ok(value) = HeaderValue::from_str(&dashboard.signer.clear(SESSION_COOKIE)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}
