//! The ChatGPT sign-in's token manager (#620), against a stub token endpoint.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use common::{Dir, NOW, credentials, manager, open, read, renewal, requests, save, text};
use henk_llm::{LlmError, TokenSource};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn a_token_that_is_good_is_returned_without_asking_openai() {
    let server = MockServer::start().await;
    let dir = Dir::new();
    let (manager, _) = manager(
        &server,
        &dir,
        &credentials("access-1", "refresh-1", NOW + 3000),
    );

    assert_eq!(text(&manager.bearer().await.unwrap()), "access-1");

    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn an_expiring_token_is_renewed_once_and_the_new_pair_is_saved() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-2", "refresh-2"))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let (manager, _) = manager(
        &server,
        &dir,
        &credentials("access-1", "refresh-1", NOW + 100),
    );

    assert_eq!(text(&manager.bearer().await.unwrap()), "access-2");
    assert_eq!(text(&manager.bearer().await.unwrap()), "access-2");

    let sent = requests(&server).await;
    assert_eq!(
        sent,
        ["grant_type=refresh_token&refresh_token=refresh-1&client_id=client-1"],
        "renewed once, with the refresh token and the issued client"
    );
    let saved = read(&dir.file());
    assert_eq!(text(&saved.refresh_token), "refresh-2");
    assert_eq!(text(&saved.access_token), "access-2");
    assert_eq!(saved.expires_at, NOW + 3600);
    assert_eq!(saved.refreshed_at, NOW);
    assert_eq!(saved.client_id, "client-1");
    assert_eq!(saved.account.as_deref(), Some("someone@example.com"));
    assert_eq!(
        std::fs::metadata(dir.file()).unwrap().permissions().mode() & 0o777,
        0o600,
        "owner only"
    );
    let names: Vec<_> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "no temporary file is left: {names:?}");
}

#[tokio::test]
async fn sixteen_callers_at_once_share_one_renewal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-2", "refresh-2").set_delay(Duration::from_millis(150)))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let (manager, _) = manager(
        &server,
        &dir,
        &credentials("access-1", "refresh-1", NOW - 10),
    );

    let calls: Vec<_> = (0..16)
        .map(|_| {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.bearer().await })
        })
        .collect();
    for call in calls {
        assert_eq!(text(&call.await.unwrap().unwrap()), "access-2");
    }

    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn the_rotated_refresh_token_is_on_disk_before_the_token_is_used() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-2", "refresh-2"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-3", "refresh-3"))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let (first, clock) = manager(
        &server,
        &dir,
        &credentials("access-1", "refresh-1", NOW + 100),
    );

    let token = first.bearer().await.unwrap();
    assert_eq!(text(&token), "access-2");
    assert_eq!(
        text(&read(&dir.file()).refresh_token),
        "refresh-2",
        "on disk by the time the caller has the token"
    );

    // Henk stops right there and starts again: the next renewal uses the
    // rotated token, not the one it started with.
    drop(first);
    clock.store(NOW + 4000, Ordering::SeqCst);
    let second = open(&server, &dir.file(), &clock);
    assert_eq!(text(&second.bearer().await.unwrap()), "access-3");

    let sent = requests(&server).await;
    assert!(sent[0].contains("refresh_token=refresh-1&"), "{sent:?}");
    assert!(sent[1].contains("refresh_token=refresh-2&"), "{sent:?}");
    assert_eq!(text(&read(&dir.file()).refresh_token), "refresh-3");
}

#[tokio::test]
async fn nothing_is_handed_out_until_the_renewed_pair_can_be_saved() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-2", "refresh-2"))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let (manager, _) = manager(
        &server,
        &dir,
        &credentials("access-1", "refresh-1", NOW + 100),
    );
    // A directory nobody can write to (a user who can is not stopped by it).
    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::File::create(dir.0.join("probe")).is_ok() {
        return;
    }

    let error = manager.bearer().await.unwrap_err();
    assert!(
        matches!(error, LlmError::CredentialFile { .. }),
        "no token while it cannot be saved: {error:?}"
    );
    assert!(!error.to_string().contains("access-2"));

    std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        text(&manager.bearer().await.unwrap()),
        "access-2",
        "saved at the next call, and the token handed out then"
    );
    assert_eq!(text(&read(&dir.file()).refresh_token), "refresh-2");
    assert_eq!(requests(&server).await.len(), 1, "renewed once, not again");
}

#[tokio::test]
async fn a_revoked_refresh_token_signs_the_endpoint_out_and_asks_openai_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant",
            "error_description": "refresh-1 was revoked",
        })))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let (manager, _) = manager(
        &server,
        &dir,
        &credentials("access-1", "refresh-1", NOW - 10),
    );

    for _ in 0..5 {
        let error = manager.bearer().await.unwrap_err();
        let LlmError::SignedOut { endpoint, reason } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(
            (endpoint.as_str(), reason.as_str()),
            ("chatgpt", "invalid_grant")
        );
        let message = error.to_string();
        assert!(message.contains("endpoint chatgpt"), "{message}");
        assert!(message.contains("sign in again"), "{message}");
        assert!(!message.contains("refresh-1") && !message.contains("access-1"));
        assert!(!error.is_retryable());
    }
    assert_eq!(requests(&server).await.len(), 1, "no loop");
    assert_eq!(
        text(&read(&dir.file()).refresh_token),
        "refresh-1",
        "the file is left as it was"
    );

    // A new sign-in replaces the file; a running Henk takes it up.
    save(
        &dir.file(),
        &credentials("access-9", "refresh-9", NOW + 3000),
    );
    assert_eq!(text(&manager.bearer().await.unwrap()), "access-9");
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn every_word_for_a_refresh_token_that_is_gone_signs_out() {
    for code in [
        "invalid_grant",
        "invalid_refresh_token",
        "token_expired",
        "refresh_token_expired",
        "refresh_token_invalidated",
        "refresh_token_reused",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error": {"code": code, "message": "m"}})),
            )
            .mount(&server)
            .await;
        let dir = Dir::new();
        let (manager, _) = manager(&server, &dir, &credentials("a", "r", NOW - 1));

        let error = manager.bearer().await.unwrap_err();

        assert!(
            matches!(&error, LlmError::SignedOut { reason, .. } if reason == code),
            "{code}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_failing_token_endpoint_is_retryable_and_leaves_the_file_alone() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-2", "refresh-2"))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let original = credentials("access-1", "refresh-1", NOW - 10);
    let (manager, _) = manager(&server, &dir, &original);
    let before = std::fs::read(dir.file()).unwrap();

    let down = manager.bearer().await.unwrap_err();
    assert!(
        matches!(down, LlmError::Server { status: 503, .. }) && down.is_retryable(),
        "{down:?}"
    );
    let busy = manager.bearer().await.unwrap_err();
    assert!(matches!(busy, LlmError::RateLimited { .. }), "{busy:?}");
    assert_eq!(std::fs::read(dir.file()).unwrap(), before, "untouched");

    assert_eq!(
        text(&manager.bearer().await.unwrap()),
        "access-2",
        "not signed out by the failures"
    );
}

#[tokio::test]
async fn an_unreadable_answer_is_a_decode_error_not_a_sign_out() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>hi</html>"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-2", "refresh-2"))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let (manager, _) = manager(&server, &dir, &credentials("a", "r", NOW - 1));

    let error = manager.bearer().await.unwrap_err();
    assert!(matches!(error, LlmError::Decode(_)), "{error:?}");
    assert_eq!(text(&manager.bearer().await.unwrap()), "access-2");
}

#[tokio::test]
async fn a_token_the_api_refused_is_renewed_at_the_next_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(renewal("access-2", "refresh-2"))
        .mount(&server)
        .await;
    let dir = Dir::new();
    let (manager, _) = manager(
        &server,
        &dir,
        &credentials("access-1", "refresh-1", NOW + 3000),
    );

    manager
        .rejected(&"some-other-token".to_owned().into())
        .await;
    assert_eq!(
        text(&manager.bearer().await.unwrap()),
        "access-1",
        "not ours"
    );
    assert!(requests(&server).await.is_empty());

    manager.rejected(&"access-1".to_owned().into()).await;
    assert_eq!(text(&manager.bearer().await.unwrap()), "access-2");
    assert_eq!(requests(&server).await.len(), 1);
    assert_eq!(text(&manager.bearer().await.unwrap()), "access-2");
    assert_eq!(requests(&server).await.len(), 1, "once");
}

#[tokio::test]
async fn no_sign_in_file_and_an_unusable_one_are_told_apart() {
    let server = MockServer::start().await;
    let dir = Dir::new();
    let clock = Arc::new(AtomicI64::new(NOW));
    let manager = open(&server, &dir.file(), &clock);

    let missing = manager.bearer().await.unwrap_err();
    assert!(
        matches!(&missing, LlmError::SignedOut { reason, .. } if reason == "no sign-in file yet"),
        "{missing:?}"
    );

    std::fs::write(dir.file(), r#"{"refresh_token": "refresh-secret", "oops"#).unwrap();
    let broken = manager.bearer().await.unwrap_err();
    assert!(
        matches!(broken, LlmError::CredentialFile { .. }),
        "{broken:?}"
    );
    assert!(
        !broken.to_string().contains("refresh-secret"),
        "the file's text is not quoted: {broken}"
    );

    let mut newer: serde_json::Value =
        serde_json::from_str(&credentials("a", "r", NOW + 3000).to_json().unwrap()).unwrap();
    newer["version"] = serde_json::json!(2);
    std::fs::write(dir.file(), newer.to_string()).unwrap();
    let error = manager.bearer().await.unwrap_err();
    assert!(error.to_string().contains("version 2"), "{error}");
}
