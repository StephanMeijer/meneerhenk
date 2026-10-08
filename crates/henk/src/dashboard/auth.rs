//! Sign-in with GitHub. GitHub tells Henk who someone is; the configured
//! list of GitHub user ids decides whether they may look (§2). The GitHub
//! token is used once, to read the account, and then dropped.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Form, FromRequest, FromRequestParts, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ring::rand::{SecureRandom as _, SystemRandom};
use secrecy::ExposeSecret as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use subtle::ConstantTimeEq as _;
use tracing::warn;

use super::Dashboard;
use super::api::ApiError;
use super::session::{SESSION_COOKIE, STATE_COOKIE, Session, cookie_value};
use crate::pages::{Notice, notice_page};

/// Someone signed in and allowed. Every page of the dashboard app takes
/// one; without it the browser goes to sign-in and back.
#[derive(Debug, Clone, Copy)]
pub struct Viewer;

impl FromRequestParts<Arc<Dashboard>> for Viewer {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        dashboard: &Arc<Dashboard>,
    ) -> Result<Self, Self::Rejection> {
        if signed_in(parts, dashboard).is_some() {
            return Ok(Self);
        }
        // Back to this page after sign-in, so a run link posted on a pull
        // request lands on that run (#69).
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

/// The session of the request, when genuine, current and of an id on the
/// list. The list is checked on every request, so taking an id off it ends
/// its access at once.
fn signed_in(parts: &Parts, dashboard: &Dashboard) -> Option<Session> {
    session_of(parts, dashboard).filter(|s| allowed(dashboard, s))
}

/// The request's session when genuine and current, whether or not its id
/// is still on the list.
fn session_of(parts: &Parts, dashboard: &Dashboard) -> Option<Session> {
    parts
        .headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| cookie_value(cookies, SESSION_COOKIE))
        .and_then(|value| dashboard.signer.read_session(value))
}

fn allowed(dashboard: &Dashboard, session: &Session) -> bool {
    dashboard
        .config
        .allowed_github_ids
        .contains(&session.github_id)
}

/// Someone signed in and allowed, for the JSON API (#198). Unlike
/// [`Viewer`] it never redirects: no session is a 401, a session whose id
/// is not on the list a 403.
#[derive(Debug, Clone)]
pub struct ApiViewer(pub Session);

impl FromRequestParts<Arc<Dashboard>> for ApiViewer {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        dashboard: &Arc<Dashboard>,
    ) -> Result<Self, Self::Rejection> {
        api_session(parts, dashboard).map(Self)
    }
}

fn api_session(parts: &Parts, dashboard: &Dashboard) -> Result<Session, ApiError> {
    let session = session_of(parts, dashboard).ok_or_else(ApiError::unauthenticated)?;
    if allowed(dashboard, &session) {
        Ok(session)
    } else {
        Err(ApiError::forbidden(format!(
            "This GitHub account (github:{}) may not use Henk's dashboard. Ask an operator to add this id.",
            session.github_id
        )))
    }
}

/// The header an API action carries its session's CSRF token in.
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Someone signed in doing something through the JSON API (#198): what
/// [`Act`] checks for a form, for a JSON body. The request comes from the
/// dashboard's own origin, carries the session's CSRF token in
/// [`CSRF_HEADER`], and its body is JSON, which a form on another site
/// cannot send. Any failure is a 403 or a 4xx for the body, and nothing
/// happens.
#[derive(Debug)]
pub struct ApiAct<T> {
    /// Who acts.
    pub session: Session,
    /// The body.
    pub body: T,
}

impl<T> FromRequest<Arc<Dashboard>> for ApiAct<T>
where
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, dashboard: &Arc<Dashboard>) -> Result<Self, ApiError> {
        let (parts, body) = request.into_parts();
        let session = api_session(&parts, dashboard)?;
        if !from_the_dashboard(
            &parts.headers,
            &dashboard.app.settings.server.public_base_url,
        ) {
            warn!(
                github_id = session.github_id,
                "an API action came from another origin"
            );
            return Err(ApiError::forbidden(
                "This request did not come from Henk's dashboard.",
            ));
        }
        let token = parts
            .headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !dashboard.signer.csrf_matches(&session, token) {
            warn!(
                github_id = session.github_id,
                "an API action without its session's token"
            );
            return Err(ApiError::csrf(
                "The CSRF token is missing or not from this session. Reload and try again.",
            ));
        }
        let Json(body) = Json::<T>::from_request(Request::from_parts(parts, body), &())
            .await
            .map_err(|rejection| {
                ApiError::new(rejection.status(), "bad_request", rejection.body_text())
            })?;
        Ok(Self { session, body })
    }
}

/// A signed-in person posting a form of the dashboard: since the app acts
/// through the API ([`ApiAct`]), only sign-out (#69). The request must
/// come from the dashboard itself: its `Origin` (or, without one, its
/// `Referer`) is `server.public_base_url`, and its form carries the
/// session's CSRF token. `SameSite=Lax` alone does not stop a form posted
/// from another site in every browser. Any failure is a 403, never a
/// redirect, so nothing happens by accident.
#[derive(Debug, Clone, Copy)]
pub struct Act;

/// A form with nothing but its CSRF token.
#[derive(Debug, Deserialize)]
struct TokenOnly {
    csrf: Option<String>,
}

impl FromRequest<Arc<Dashboard>> for Act {
    type Rejection = Response;

    async fn from_request(request: Request, dashboard: &Arc<Dashboard>) -> Result<Self, Response> {
        let (parts, body) = request.into_parts();
        let Some(session) = signed_in(&parts, dashboard) else {
            return Err(refused("Sign in first."));
        };
        if !from_the_dashboard(
            &parts.headers,
            &dashboard.app.settings.server.public_base_url,
        ) {
            warn!(
                github_id = session.github_id,
                "a dashboard form came from another origin"
            );
            return Err(refused("This request did not come from Henk's dashboard."));
        }
        let Form(form) = Form::<TokenOnly>::from_request(Request::from_parts(parts, body), &())
            .await
            .map_err(|rejection| {
                notice(
                    StatusCode::BAD_REQUEST,
                    "Not understood",
                    &rejection.body_text(),
                )
            })?;
        let token = form.csrf.unwrap_or_default();
        if !dashboard.signer.csrf_matches(&session, &token) {
            warn!(
                github_id = session.github_id,
                "a dashboard form without its session's token"
            );
            return Err(refused(
                "This form is out of date or not from your session. Reload the page and try again.",
            ));
        }
        Ok(Self)
    }
}

fn refused(text: &str) -> Response {
    notice(StatusCode::FORBIDDEN, "Not done", text)
}

/// Whether a request names the dashboard's own origin: `Origin` when the
/// browser sent one, else `Referer`. A request with neither is refused.
pub(super) fn from_the_dashboard(headers: &HeaderMap, public_base_url: &str) -> bool {
    let origin = origin_of(public_base_url);
    let text = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };
    match text(header::ORIGIN) {
        Some(sent) => sent.eq_ignore_ascii_case(origin),
        None => text(header::REFERER).is_some_and(|referer| {
            referer
                .get(..origin.len() + 1)
                .is_some_and(|start| start.eq_ignore_ascii_case(&format!("{origin}/")))
        }),
    }
}

/// `scheme://host[:port]` of a URL.
fn origin_of(url: &str) -> &str {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url;
    };
    let authority = rest.find('/').unwrap_or(rest.len());
    url.get(..scheme.len() + 3 + authority).unwrap_or(url)
}

pub(super) fn redirect(to: &str, cookie: Option<String>) -> Response {
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

pub(super) fn notice(status: StatusCode, title: &str, text: &str) -> Response {
    say(
        status,
        &Notice {
            title,
            text,
            ..Notice::default()
        },
    )
}

/// A notice page with more to it: an id to pass on, a way on (#232).
fn say(status: StatusCode, notice: &Notice<'_>) -> Response {
    (status, notice_page(notice)).into_response()
}

/// Where a failed sign-in goes on: a fresh one.
const SIGN_IN: (&str, &str) = ("Sign in with GitHub", "/dashboard/login");

/// A sign-in that did not go through, with the way to try again.
fn sign_in_failed(status: StatusCode, text: &str) -> Response {
    say(
        status,
        &Notice {
            title: "Sign-in failed",
            text,
            action: Some(SIGN_IN),
            ..Notice::default()
        },
    )
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
        return sign_in_failed(
            StatusCode::BAD_REQUEST,
            "The sign-in expired or did not start here. Try again.",
        );
    };
    if !bool::from(state.as_bytes().ct_eq(expected.as_bytes())) {
        return sign_in_failed(
            StatusCode::BAD_REQUEST,
            "The sign-in did not start here. Try again.",
        );
    }
    let user = match github_user(&dashboard, &code).await {
        Ok(user) => user,
        Err(error) => {
            warn!(%error, "GitHub sign-in failed");
            return sign_in_failed(
                StatusCode::BAD_GATEWAY,
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
        let framed = format!(
            "Signed in at GitHub as github:{}. Ask an operator to add this id.",
            user.github_id
        );
        let mut response = say(
            StatusCode::FORBIDDEN,
            &Notice {
                title: "Not for you",
                text: "This GitHub account may not see Henk's dashboard.",
                framed: Some(&framed),
                action: Some(("Sign in again", "/dashboard/login")),
                hint: Some("To use another GitHub account, sign out of GitHub first."),
            },
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
    let github_id = user
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("GitHub user without id"))?;
    let login = user
        .get("login")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Session::fresh(github_id, login).ok_or_else(|| anyhow::anyhow!("no randomness for a session"))
}

/// Signs out. A form like every other action, so another site cannot sign
/// someone out.
pub async fn logout(State(dashboard): State<Arc<Dashboard>>, _act: Act) -> Response {
    let mut response = say(
        StatusCode::OK,
        &Notice {
            title: "Signed out",
            text: "You are signed out of Henk's dashboard.",
            action: Some(("Sign in", "/dashboard/login")),
            ..Notice::default()
        },
    );
    if let Ok(value) = HeaderValue::from_str(&dashboard.signer.clear(SESSION_COOKIE)) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}
