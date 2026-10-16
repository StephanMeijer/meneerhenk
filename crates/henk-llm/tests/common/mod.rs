//! Helpers of the ChatGPT sign-in tests.

#![allow(dead_code, clippy::unwrap_used)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use henk_llm::build_endpoint_client;
use henk_llm::chatgpt::{Credentials, TokenManager};
use secrecy::{ExposeSecret as _, SecretString};
use wiremock::{MockServer, ResponseTemplate};

pub const NOW: i64 = 1_790_000_000;

/// A directory under the temp directory, removed with the guard.
pub struct Dir(pub PathBuf);

impl Dir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("henk-chatgpt-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn file(&self) -> PathBuf {
        self.0.join("openai.json")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn credentials(access: &str, refresh: &str, expires_at: i64) -> Credentials {
    Credentials {
        client_id: "client-1".to_owned(),
        ext_agent_host_id: "host-1".to_owned(),
        access_token: access.to_owned().into(),
        refresh_token: refresh.to_owned().into(),
        id_token: Some("id-token-1".to_owned().into()),
        expires_at,
        earliest_refresh_at: None,
        refreshed_at: NOW - 3000,
        scopes: vec!["offline_access".to_owned()],
        account: Some("someone@example.com".to_owned()),
    }
}

pub fn save(path: &Path, credentials: &Credentials) {
    std::fs::write(path, credentials.to_json().unwrap()).unwrap();
}

pub fn read(path: &Path) -> Credentials {
    Credentials::from_json(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A manager over a file holding `credentials`, and the clock it reads.
pub fn manager(
    server: &MockServer,
    dir: &Dir,
    credentials: &Credentials,
) -> (Arc<TokenManager>, Arc<AtomicI64>) {
    save(&dir.file(), credentials);
    let clock = Arc::new(AtomicI64::new(NOW));
    (open(server, &dir.file(), &clock), clock)
}

pub fn open(server: &MockServer, file: &Path, clock: &Arc<AtomicI64>) -> Arc<TokenManager> {
    let clock = Arc::clone(clock);
    Arc::new(
        TokenManager::new("chatgpt", file, build_endpoint_client().unwrap())
            .with_token_url(format!("{}/oauth/token", server.uri()))
            .with_clock(Arc::new(move || clock.load(Ordering::SeqCst))),
    )
}

pub fn renewal(access: &str, refresh: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "id_token": "id-token-2",
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": "openid offline_access chatgpt.tokens.use.direct",
    }))
}

pub async fn requests(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| String::from_utf8(r.body.clone()).unwrap())
        .collect()
}

pub fn text(secret: &SecretString) -> &str {
    secret.expose_secret()
}
