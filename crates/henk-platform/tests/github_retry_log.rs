//! A retry is logged at WARN with its status, attempt and endpoint (#338).
//! Its own test binary: a subscriber set for one test sees nothing while
//! other tests' threads share the log sites' cached interest.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use henk_platform::github::{GitHubApi, GitHubAuth};
use henk_platform::retry::RetryPolicy;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Collects what is logged, as text.
#[derive(Clone, Default)]
struct Logged(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logged {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn each_retry_is_logged_with_its_status_attempt_and_endpoint() {
    let logged = Logged::default();
    let sink = logged.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/repos/docspec/app/check-runs/5"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/docspec/app/check-runs/5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 5})))
        .mount(&server)
        .await;
    GitHubApi::new(&server.uri(), GitHubAuth::token("t".to_owned().into()))
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        })
        .start_check_run("docspec", "app", 5, "https://henk/runs/r-1")
        .await
        .unwrap();

    let text = String::from_utf8(logged.0.lock().unwrap().clone()).unwrap();
    for said in [
        "WARN",
        "retrying platform request",
        "endpoint=\"PATCH /repos/docspec/app/check-runs/5\"",
        "attempt=1",
        "status=502",
    ] {
        assert!(text.contains(said), "{said}: {text}");
    }
}
