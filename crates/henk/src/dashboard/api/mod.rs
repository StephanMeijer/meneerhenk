//! The dashboard's JSON API (#198), at `/dashboard/api/v1`: what the dashboard shows
//! and does, as JSON, for the single-page app (#197). Every route needs a
//! signed-in, allowed viewer ([`ApiViewer`]); every action also its
//! session's CSRF token, the dashboard's own origin and a JSON body
//! ([`ApiAct`]). Errors are JSON too, and nothing redirects. Nothing here
//! returns a secret. `docs/API.md` lists the routes.

mod events;
mod runs;
#[cfg(test)]
mod tests;
pub mod types;

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use henk_store::StoreError;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::warn;

use super::Dashboard;
use super::auth::ApiViewer;
use super::views::health_rows;
use types::{ErrorBody, ErrorDetail, Health, HealthCheck, Me};

/// Rows per page when a listing does not say.
const DEFAULT_LIMIT: u32 = 50;

/// An API error: a status and a JSON body with a code and a message.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    /// An error with this status, code and message.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// No session, or not a current one.
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "Sign in first.",
        )
    }

    /// Not allowed to do this.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    /// The request is not one the API understands.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    /// There is nothing at this path.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// The store failed. The detail goes to the log, not the client.
    fn store(error: &StoreError) -> Self {
        warn!(%error, "the API could not read the run store");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "store",
            "The run store failed; the log says why.",
        )
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        Self::store(&error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = if self.status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            "unsupported_media_type"
        } else {
            self.code
        };
        let body = ErrorBody {
            error: ErrorDetail {
                code: code.to_owned(),
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

/// What a handler returns.
type ApiResult<T> = Result<Json<T>, ApiError>;

/// Every API route, to be nested at `/dashboard/api/v1`, inside the
/// session cookie's path.
pub fn routes(dashboard: Arc<Dashboard>) -> Router {
    Router::new()
        .route("/me", get(me))
        .route("/health", get(health))
        .route("/runs", get(runs::list).post(runs::start))
        .route("/runs/{id}", get(runs::detail))
        .route("/runs/{id}/events", get(runs::events))
        .route("/runs/{id}/tool-calls", get(runs::tool_calls))
        .route("/runs/{id}/transcripts/{session}", get(runs::transcript))
        .route("/runs/{id}/cancel", post(runs::cancel))
        .route("/events", get(events::list))
        .route("/events/{id}", get(events::detail))
        .fallback(not_found)
        .layer(middleware::map_response(no_store))
        .with_state(dashboard)
}

/// API responses are never cached and never sniffed as anything but JSON.
async fn no_store(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn not_found() -> ApiError {
    ApiError::not_found("No such API route.")
}

/// Who is signed in, and their CSRF token.
async fn me(State(dashboard): State<Arc<Dashboard>>, ApiViewer(viewer): ApiViewer) -> Json<Me> {
    Json(Me {
        github_id: viewer.github_id,
        csrf: dashboard.signer.csrf(&viewer),
        login: viewer.login,
    })
}

/// What `/dashboard/health` shows.
async fn health(State(dashboard): State<Arc<Dashboard>>, _viewer: ApiViewer) -> Json<Health> {
    let checks = health_rows(&dashboard)
        .await
        .into_iter()
        .map(|(name, state, detail)| HealthCheck {
            name,
            state,
            detail,
        })
        .collect();
    Json(Health { checks })
}

/// A page size from `?limit=`: the default when absent, at most
/// [`henk_store::Page::MAX`].
fn limit(given: Option<u32>) -> u32 {
    given
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, henk_store::Page::MAX)
}

/// A cursor for the row after (`time`, `id`) in a newest-first listing.
fn cursor(time: &str, id: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{time}|{id}"))
}

/// The (time, id) a cursor names, or why it is not one.
fn read_cursor(cursor: &str) -> Result<(String, String), ApiError> {
    let bad = || ApiError::bad_request("The cursor is not one this API gave out.");
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| bad())?;
    let text = String::from_utf8(bytes).map_err(|_| bad())?;
    let (time, id) = text.split_once('|').ok_or_else(bad)?;
    OffsetDateTime::parse(time, &Rfc3339).map_err(|_| bad())?;
    if id.is_empty() {
        return Err(bad());
    }
    Ok((time.to_owned(), id.to_owned()))
}

/// An RFC 3339 time from a query, in the form the store writes, or why
/// it is not one.
fn read_time(name: &str, value: &str) -> Result<String, ApiError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .and_then(|at| at.to_offset(time::UtcOffset::UTC).format(&Rfc3339).ok())
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "{name} is not an RFC 3339 time such as 2026-10-07T12:00:00Z."
            ))
        })
}
