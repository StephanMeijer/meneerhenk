//! The dashboard app (#199, #201): the single-page app in `dashboard/`,
//! built by Vite and embedded at compile time by `build.rs`. It is the
//! dashboard: served at `/dashboard` behind the sign-in, every path under
//! it that is not the API or the sign-in is the app, and it talks to Henk
//! only through `/dashboard/api/v1`. Its files come from a table in
//! memory, never from the disk, and every response forbids inline script
//! and style, framing and sniffing.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::Dashboard;
use super::auth::Viewer;

/// Where the app is served.
pub const BASE: &str = "/dashboard";

/// The app's files: a path under [`BASE`] and its bytes.
#[derive(Debug, Clone, Copy)]
pub struct Assets(pub &'static [(&'static str, &'static [u8])]);

impl Assets {
    /// The files `build.rs` embedded; none when `dashboard/dist` was not
    /// built.
    #[must_use]
    pub fn built() -> Self {
        Self(include!(concat!(env!("OUT_DIR"), "/dashboard_assets.rs")))
    }

    fn get(self, path: &str) -> Option<&'static [u8]> {
        self.0
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| *bytes)
    }
}

/// The policy every app response carries: no inline script or style, no
/// other origin, no framing.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// `/dashboard` and `/dashboard/`: the app. Its asset paths are absolute,
/// so both work.
pub async fn index(State(dashboard): State<Arc<Dashboard>>, _viewer: Viewer) -> Response {
    serve(dashboard.assets, "")
}

/// `/dashboard/{*path}`: a file of the app, or the app itself for a path
/// it routes in the browser, such as `/dashboard/runs/r-1`.
pub async fn file(
    State(dashboard): State<Arc<Dashboard>>,
    _viewer: Viewer,
    Path(path): Path<String>,
) -> Response {
    serve(dashboard.assets, &path)
}

fn serve(assets: Assets, path: &str) -> Response {
    if assets.0.is_empty() {
        return with_headers(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The dashboard app was not built into this binary: run `npm ci && npm run build` in dashboard/, then build Henk again.",
            )
                .into_response(),
        );
    }
    let unsafe_part = path
        .split('/')
        .any(|part| part == ".." || part == "." || part.contains('\\'))
        || path.starts_with('/');
    if unsafe_part {
        return not_found();
    }
    if let Some(bytes) = (!path.is_empty()).then(|| assets.get(path)).flatten() {
        let cache = if path.starts_with("assets/") {
            // Vite puts a hash of the content in these names.
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return file_response(bytes, content_type(path), cache);
    }
    // Under assets/ only a file Vite built exists. Anything else is a route
    // of the app, which shows its own not-found view; a route may hold a
    // dot, such as a session named lane.a.
    if path.starts_with("assets/") {
        return not_found();
    }
    match assets.get("index.html") {
        Some(bytes) => file_response(bytes, content_type("index.html"), "no-cache"),
        None => not_found(),
    }
}

fn file_response(
    bytes: &'static [u8],
    content_type: &'static str,
    cache: &'static str,
) -> Response {
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    with_headers(response)
}

fn not_found() -> Response {
    with_headers((StatusCode::NOT_FOUND, "Not found.").into_response())
}

/// The security headers of every app response.
fn with_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// A file's type, by its extension.
fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use super::{Assets, CSP};
    use crate::dashboard::session::Session;
    use crate::dashboard::tests::{ALLOWED, Fixture, fixture_with_assets, signed_in_as};

    const INDEX: &[u8] = b"<!doctype html><title>app</title>";
    const ASSETS: Assets = Assets(&[
        ("assets/app-abc.css", b"body{}"),
        ("assets/app-abc.js", b"console.log(1)"),
        ("favicon.svg", b"<svg/>"),
        ("index.html", INDEX),
    ]);

    struct Answer {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: Vec<u8>,
    }

    async fn call(f: &Fixture, method: Method, uri: &str, cookie: Option<&str>) -> Answer {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let response = f
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Answer {
            status,
            headers,
            body,
        }
    }

    async fn get(f: &Fixture, uri: &str, cookie: Option<&str>) -> Answer {
        call(f, Method::GET, uri, cookie).await
    }

    fn viewer(f: &Fixture) -> String {
        signed_in_as(f, &Session::fresh(ALLOWED, "alice".to_owned()).unwrap()).0
    }

    fn assert_guarded(answer: &Answer, uri: &str) {
        assert_eq!(
            answer.headers[header::CONTENT_SECURITY_POLICY],
            CSP,
            "{uri}"
        );
        assert_eq!(answer.headers[header::X_FRAME_OPTIONS], "DENY", "{uri}");
        assert_eq!(
            answer.headers[header::REFERRER_POLICY],
            "same-origin",
            "{uri}"
        );
        assert_eq!(
            answer.headers[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff",
            "{uri}"
        );
    }

    #[tokio::test]
    async fn signed_out_the_dashboard_leads_through_sign_in_back_to_the_same_place() {
        let f = fixture_with_assets("https://127.0.0.1:9", ASSETS);
        let answer = get(&f, "/dashboard/runs/r-1?tab=drafts", None).await;
        assert_eq!(answer.status, StatusCode::SEE_OTHER);
        assert_eq!(
            answer.headers[header::LOCATION],
            "/dashboard/login?next=%2Fdashboard%2Fruns%2Fr-1%3Ftab%3Ddrafts"
        );
        let home = get(&f, "/dashboard", None).await;
        assert_eq!(home.status, StatusCode::SEE_OTHER);
        assert_eq!(home.headers[header::LOCATION], "/dashboard/login");
        let asset = get(&f, "/dashboard/assets/app-abc.js", None).await;
        assert_eq!(
            asset.status,
            StatusCode::SEE_OTHER,
            "files need sign-in too"
        );
        let outsider = signed_in_as(&f, &Session::fresh(999, "mallory".to_owned()).unwrap()).0;
        let refused = get(&f, "/dashboard/", Some(&outsider)).await;
        assert_eq!(
            refused.status,
            StatusCode::SEE_OTHER,
            "an id off the list is signed out"
        );
        assert!(refused.body.is_empty());
    }

    #[tokio::test]
    async fn every_dashboard_page_is_the_app_and_its_files_are_served_with_their_headers() {
        let f = fixture_with_assets("https://127.0.0.1:9", ASSETS);
        let cookie = viewer(&f);

        for uri in [
            "/dashboard",
            "/dashboard/",
            "/dashboard/runs/r-1",
            "/dashboard/runs/r-1/transcripts/lane.a",
            "/dashboard/events",
            "/dashboard/events/e-1",
            "/dashboard/health/",
            "/dashboard/no/such/page",
        ] {
            let page = get(&f, uri, Some(&cookie)).await;
            assert_eq!(page.status, StatusCode::OK, "{uri}");
            assert_eq!(page.body, INDEX, "{uri} is the app");
            assert_eq!(
                page.headers[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
            assert_eq!(page.headers[header::CACHE_CONTROL], "no-cache");
            assert_guarded(&page, uri);
        }

        let script = get(&f, "/dashboard/assets/app-abc.js", Some(&cookie)).await;
        assert_eq!(script.status, StatusCode::OK);
        assert_eq!(script.body, b"console.log(1)");
        assert_eq!(
            script.headers[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            script.headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_guarded(&script, "script");
        let style = get(&f, "/dashboard/assets/app-abc.css", Some(&cookie)).await;
        assert_eq!(
            style.headers[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        let icon = get(&f, "/dashboard/favicon.svg", Some(&cookie)).await;
        assert_eq!(icon.headers[header::CONTENT_TYPE], "image/svg+xml");
        assert_eq!(icon.headers[header::CACHE_CONTROL], "no-cache");
    }

    #[tokio::test]
    async fn a_missing_file_or_a_path_trick_is_not_found_never_the_app() {
        let f = fixture_with_assets("https://127.0.0.1:9", ASSETS);
        let cookie = viewer(&f);
        for uri in [
            "/dashboard/assets/missing.js",
            "/dashboard/assets/deeper/route",
            "/dashboard/runs/..%2F..%2Fetc%2Fpasswd",
            "/dashboard/..%2Findex.html",
            "/dashboard/a%5Cb",
            "/dashboard//etc/passwd",
            "/dashboard/./index.html",
        ] {
            let answer = get(&f, uri, Some(&cookie)).await;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{uri}");
            assert_ne!(answer.body, INDEX, "{uri}");
            assert_guarded(&answer, uri);
        }
    }

    #[tokio::test]
    async fn the_api_and_sign_in_keep_their_routes_and_the_old_pages_actions_are_gone() {
        let f = fixture_with_assets("https://127.0.0.1:9", ASSETS);
        let cookie = viewer(&f);

        let api = get(&f, "/dashboard/api/v1/nope", Some(&cookie)).await;
        assert_eq!(api.status, StatusCode::NOT_FOUND);
        let api: serde_json::Value = serde_json::from_slice(&api.body).unwrap();
        assert_eq!(
            api["error"]["code"], "not_found",
            "the API keeps its JSON 404"
        );
        let me = get(&f, "/dashboard/api/v1/me", Some(&cookie)).await;
        assert_eq!(me.status, StatusCode::OK);
        assert_ne!(me.body, INDEX);

        let login = get(&f, "/dashboard/login", None).await;
        assert_eq!(
            login.status,
            StatusCode::SEE_OTHER,
            "sign-in goes to GitHub"
        );
        assert!(
            login.headers[header::LOCATION]
                .to_str()
                .unwrap()
                .contains("/login/oauth/authorize")
        );

        for uri in ["/dashboard/start", "/dashboard/runs/r-1/cancel"] {
            let posted = call(&f, Method::POST, uri, Some(&cookie)).await;
            assert_eq!(
                posted.status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{uri}: actions are the API's now"
            );
        }
        for uri in ["/dashboard/running.json", "/dashboard/app.js"] {
            let gone = get(&f, uri, Some(&cookie)).await;
            assert_eq!(
                gone.body, INDEX,
                "{uri} is no longer served: the app answers"
            );
        }
    }

    #[tokio::test]
    async fn without_a_built_app_henk_says_how_to_build_it() {
        let f = fixture_with_assets("https://127.0.0.1:9", Assets(&[]));
        let cookie = viewer(&f);
        let answer = get(&f, "/dashboard/", Some(&cookie)).await;
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(String::from_utf8_lossy(&answer.body).contains("npm run build"));
        assert_guarded(&answer, "unbuilt");
    }
}
