//! The browser sign-in (#620) against a stand-in for OpenAI: a token
//! endpoint and a key set on wiremock, and the test playing the browser by
//! calling the loopback address the sign-in listens on.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

mod signin_support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use henk_llm::build_endpoint_client;
use henk_llm::chatgpt::{Credentials, Pending, SignIn, SignInError};
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use signin_support::{header, jwks, sign};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NOW: i64 = 1_790_000_000;
const ISSUED: &str = "oaiapp_issued_for_this_user";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

/// The stand-in for OpenAI's side.
struct Openai {
    server: MockServer,
}

impl Openai {
    async fn start() -> Self {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks()))
            .mount(&server)
            .await;
        Self { server }
    }

    fn flow(&self) -> SignIn {
        henk_llm::ensure_tls_provider();
        SignIn::new(build_endpoint_client().unwrap())
            .with_urls(
                "https://auth.example.test/authorize",
                format!("{}/token", self.server.uri()),
                format!("{}/jwks", self.server.uri()),
            )
            .with_clock(Arc::new(|| NOW))
    }

    /// Answers the code exchange with `body`.
    async fn answers(&self, status: u16, body: &Value) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// The form fields of the code exchanges received.
    async fn exchanges(&self) -> Vec<HashMap<String, String>> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/token")
            .map(|request| {
                let text = String::from_utf8(request.body.clone()).unwrap();
                reqwest::Url::parse(&format!("http://form.test/?{text}"))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect()
            })
            .collect()
    }
}

/// The query of the URL the browser is sent to.
fn query_of(url: &str) -> HashMap<String, String> {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// An ID token for the attempt whose authorization URL is `url`, with
/// `change` applied to its claims.
fn id_token_for(url: &str, change: impl FnOnce(&mut Value)) -> String {
    let query = query_of(url);
    let mut claims = json!({
        "iss": "https://auth.openai.com",
        "aud": ISSUED,
        "sub": "user-123",
        "email": "someone@example.com",
        "nonce": query["nonce"],
        "exp": NOW + 3600,
        "iat": NOW,
    });
    change(&mut claims);
    sign(&header(), &claims)
}

fn tokens(id_token: &str) -> Value {
    json!({
        "access_token": "access-new",
        "refresh_token": "refresh-new",
        "id_token": id_token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": SCOPES,
        "earliest_refresh_at": NOW + 3000,
    })
}

/// The browser coming back to `redirect_uri` with `params`.
async fn come_back(redirect_uri: &str, params: &[(&str, &str)]) -> reqwest::StatusCode {
    let mut url = reqwest::Url::parse(redirect_uri).unwrap();
    url.query_pairs_mut().extend_pairs(params);
    reqwest::Client::new()
        .get(url)
        .send()
        .await
        .unwrap()
        .status()
}

/// The browser coming back to `pending` with `params`. The future owns what
/// it needs, so `pending` can be finished meanwhile.
fn browser(
    pending: &Pending,
    params: &[(String, String)],
) -> impl std::future::Future<Output = reqwest::StatusCode> + use<> {
    let redirect_uri = pending.redirect_uri().to_owned();
    let params = params.to_vec();
    async move { come_back(&redirect_uri, &as_refs(&params)).await }
}

/// The parameters of a good callback for `pending`.
fn good_callback(pending: &Pending) -> Vec<(String, String)> {
    let query = query_of(pending.authorization_url());
    vec![
        ("code".to_owned(), "the-one-time-code".to_owned()),
        ("state".to_owned(), query["state"].clone()),
        ("client_id".to_owned(), ISSUED.to_owned()),
        ("scope".to_owned(), SCOPES.to_owned()),
    ]
}

fn as_refs(params: &[(String, String)]) -> Vec<(&str, &str)> {
    params
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// Runs a sign-in whose browser comes back with `callback(pending)`.
async fn sign_in(
    openai: &Openai,
    existing: Option<Credentials>,
    callback: impl FnOnce(&Pending) -> Vec<(String, String)>,
) -> (Result<Credentials, SignInError>, String) {
    let pending = openai.flow().start(existing, 0).await.unwrap();
    let url = pending.authorization_url().to_owned();
    let params = callback(&pending);
    let (_, result) = tokio::join!(
        browser(&pending, &params),
        pending.finish(Duration::from_secs(10))
    );
    (result, url)
}

fn existing() -> Credentials {
    Credentials {
        client_id: ISSUED.to_owned(),
        ext_agent_host_id: "urn:uuid:11111111-2222-4333-8444-555555555555".to_owned(),
        access_token: "access-old".to_owned().into(),
        refresh_token: "refresh-old".to_owned().into(),
        id_token: Some("id-token-old".to_owned().into()),
        expires_at: NOW - 100,
        earliest_refresh_at: None,
        refreshed_at: NOW - 3700,
        scopes: vec!["offline_access".to_owned()],
        account: Some("someone@example.com".to_owned()),
    }
}

#[tokio::test]
async fn a_first_sign_in_registers_and_leaves_the_credentials_to_save() {
    let openai = Openai::start().await;
    let pending = openai.flow().start(None, 0).await.unwrap();
    let url = pending.authorization_url().to_owned();
    let query = query_of(&url);

    // The page the browser is sent to.
    assert!(url.starts_with("https://auth.example.test/authorize?"));
    assert_eq!(query["client_id"], "dynamic_agent_client");
    assert_eq!(query["agent_name_hint"], "Henk");
    assert!(query["ext_agent_host_id"].starts_with("urn:uuid:"));
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["scope"], SCOPES);
    assert_eq!(query["resource"], "https://api.openai.com/v1");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["redirect_uri"], pending.redirect_uri());
    assert!(pending.redirect_uri().starts_with("http://127.0.0.1:"));
    assert!(pending.redirect_uri().ends_with("/auth/callback"));
    assert!(query["state"].len() >= 40 && query["nonce"].len() >= 40);
    assert_ne!(query["state"], query["nonce"]);
    assert!(!query.contains_key("id_token_hint"));

    openai
        .answers(200, &tokens(&id_token_for(&url, |_| {})))
        .await;
    let params = good_callback(&pending);
    let (_, result) = tokio::join!(
        browser(&pending, &params),
        pending.finish(Duration::from_secs(10))
    );
    let credentials = result.unwrap();

    // What was sent for the code.
    let exchange = &openai.exchanges().await[0];
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(
        exchange["client_id"], ISSUED,
        "the issued id, not the registration's"
    );
    assert_eq!(exchange["code"], "the-one-time-code");
    assert_eq!(exchange["redirect_uri"], query["redirect_uri"]);
    assert_eq!(exchange["resource"], "https://api.openai.com/v1");
    let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        exchange["code_verifier"].as_bytes(),
    ));
    assert_eq!(
        challenge, query["code_challenge"],
        "the verifier is the challenge's"
    );

    // What is left to save.
    assert_eq!(credentials.client_id, ISSUED);
    assert_eq!(credentials.ext_agent_host_id, query["ext_agent_host_id"]);
    assert_eq!(credentials.access_token.expose_secret(), "access-new");
    assert_eq!(credentials.refresh_token.expose_secret(), "refresh-new");
    assert_eq!(credentials.expires_at, NOW + 3600);
    assert_eq!(credentials.earliest_refresh_at, Some(NOW + 3000));
    assert_eq!(credentials.refreshed_at, NOW);
    assert_eq!(credentials.account.as_deref(), Some("someone@example.com"));
    assert!(
        credentials
            .scopes
            .iter()
            .any(|s| s == "chatgpt.tokens.use.direct")
    );
}

#[tokio::test]
async fn a_repeated_sign_in_reuses_the_client_and_host_and_names_the_account() {
    let openai = Openai::start().await;
    let pending = openai.flow().start(Some(existing()), 0).await.unwrap();
    let url = pending.authorization_url().to_owned();
    let query = query_of(&url);
    assert_eq!(query["client_id"], ISSUED);
    assert!(!query.contains_key("agent_name_hint"));
    assert_eq!(query["ext_agent_host_id"], existing().ext_agent_host_id);
    assert!(
        !query.contains_key("id_token_hint"),
        "no token goes into a URL that is printed"
    );
    assert!(!url.contains("id-token-old"));
    assert!(!url.contains("refresh-old"));
    assert!(!url.contains("access-old"));
    assert_eq!(query["login_hint"], "someone@example.com");

    openai
        .answers(200, &tokens(&id_token_for(&url, |_| {})))
        .await;
    // Back without a client id: allowed when repeating.
    let mut params = good_callback(&pending);
    params.retain(|(name, _)| name != "client_id");
    let (_, result) = tokio::join!(
        browser(&pending, &params),
        pending.finish(Duration::from_secs(10))
    );
    let credentials = result.unwrap();
    assert_eq!(credentials.client_id, ISSUED);
    assert_eq!(credentials.ext_agent_host_id, existing().ext_agent_host_id);
}

#[tokio::test]
async fn a_callback_naming_another_client_is_refused_without_asking_for_tokens() {
    let openai = Openai::start().await;
    let (result, _) = sign_in(&openai, Some(existing()), |pending| {
        let mut params = good_callback(pending);
        for (name, value) in &mut params {
            if name == "client_id" {
                "oaiapp_someone_else".clone_into(value);
            }
        }
        params
    })
    .await;
    assert!(
        matches!(result, Err(SignInError::ClientChanged)),
        "{result:?}"
    );
    assert!(openai.exchanges().await.is_empty());
}

#[tokio::test]
async fn a_first_sign_in_without_an_issued_client_id_is_incomplete() {
    let openai = Openai::start().await;
    let (result, _) = sign_in(&openai, None, |pending| {
        let mut params = good_callback(pending);
        params.retain(|(name, _)| name != "client_id");
        params
    })
    .await;
    assert!(matches!(result, Err(SignInError::NoClientId)), "{result:?}");
    assert!(openai.exchanges().await.is_empty());
}

#[tokio::test]
async fn another_account_does_not_replace_the_one_in_the_file() {
    let openai = Openai::start().await;
    let pending = openai.flow().start(Some(existing()), 0).await.unwrap();
    let url = pending.authorization_url().to_owned();
    openai
        .answers(
            200,
            &tokens(&id_token_for(&url, |claims| {
                claims["email"] = json!("another@example.com");
            })),
        )
        .await;
    let params = good_callback(&pending);
    let (_, result) = tokio::join!(
        browser(&pending, &params),
        pending.finish(Duration::from_secs(10))
    );
    let error = result.unwrap_err();
    assert!(
        matches!(&error, SignInError::OtherAccount { was, now } if was == "someone@example.com" && now == "another@example.com"),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_denied_sign_in_stops_without_asking_for_tokens() {
    let openai = Openai::start().await;
    let (result, _) = sign_in(&openai, None, |pending| {
        let query = query_of(pending.authorization_url());
        vec![
            ("error".to_owned(), "access_denied".to_owned()),
            ("state".to_owned(), query["state"].clone()),
        ]
    })
    .await;
    assert!(
        matches!(&result, Err(SignInError::Denied(why)) if why == "access_denied"),
        "{result:?}"
    );
    assert!(openai.exchanges().await.is_empty());
}

#[tokio::test]
async fn a_request_that_is_not_this_sign_ins_callback_does_not_end_it() {
    let openai = Openai::start().await;
    let pending = openai.flow().start(None, 0).await.unwrap();
    let url = pending.authorization_url().to_owned();
    openai
        .answers(200, &tokens(&id_token_for(&url, |_| {})))
        .await;
    let params = good_callback(&pending);
    let base = pending.redirect_uri().to_owned();
    let own = browser(&pending, &params);
    let stray = async {
        let client = reqwest::Client::new();
        // Another state, then another path, then another Host.
        let wrong_state = come_back(&base, &[("code", "x"), ("state", "not-ours")]).await;
        let wrong_path = client
            .get(base.replace("/auth/callback", "/favicon.ico"))
            .send()
            .await
            .unwrap()
            .status();
        let no_state = come_back(&base, &[("code", "x")]).await;
        let other_host = client
            .get(format!("{base}?code=x&state={}", query_of(&url)["state"]))
            .header("host", "evil.example")
            .send()
            .await
            .unwrap()
            .status();
        let own = own.await;
        (wrong_state, wrong_path, no_state, other_host, own)
    };
    let (statuses, result) = tokio::join!(stray, pending.finish(Duration::from_secs(10)));
    assert_eq!(statuses.0, 400);
    assert_eq!(statuses.1, 404);
    assert_eq!(statuses.2, 400);
    assert_eq!(statuses.3, 400);
    assert_eq!(statuses.4, 200);
    result.unwrap();
    let exchanges = openai.exchanges().await;
    assert_eq!(exchanges.len(), 1);
    assert_eq!(exchanges[0]["code"], "the-one-time-code");
}

#[tokio::test]
async fn no_browser_in_time_is_a_timeout() {
    let openai = Openai::start().await;
    let pending = openai.flow().start(None, 0).await.unwrap();
    let result = pending.finish(Duration::from_millis(200)).await;
    assert!(
        matches!(result, Err(SignInError::TimedOut(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_port_in_use_is_named() {
    let openai = Openai::start().await;
    let first = openai.flow().start(None, 0).await.unwrap();
    let port: u16 = first
        .redirect_uri()
        .rsplit(':')
        .next()
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let error = openai.flow().start(None, port).await.unwrap_err();
    assert!(
        matches!(error, SignInError::Listen { port: p, .. } if p == port),
        "{error:?}"
    );
    assert!(error.to_string().contains("--port"));
}

#[tokio::test]
async fn a_code_the_token_endpoint_refuses_names_only_its_error() {
    let openai = Openai::start().await;
    openai
        .answers(
            400,
            &json!({"error": "invalid_grant", "error_description": "code the-one-time-code is spent"}),
        )
        .await;
    let (result, _) = sign_in(&openai, None, good_callback).await;
    let error = result.unwrap_err();
    assert!(
        matches!(&error, SignInError::Refused(why) if why.contains("invalid_grant")),
        "{error:?}"
    );
    let text = format!("{error} {error:?}");
    assert!(!text.contains("the-one-time-code"), "{text}");
    let verifier = &openai.exchanges().await[0]["code_verifier"];
    assert!(!text.contains(verifier.as_str()), "{text}");
}

#[tokio::test]
async fn without_the_plan_scope_the_sign_in_is_refused() {
    let openai = Openai::start().await;
    let pending = openai.flow().start(None, 0).await.unwrap();
    let url = pending.authorization_url().to_owned();
    let mut body = tokens(&id_token_for(&url, |_| {}));
    body["scope"] = json!("openid profile email offline_access");
    openai.answers(200, &body).await;
    let params = good_callback(&pending);
    let (_, result) = tokio::join!(
        browser(&pending, &params),
        pending.finish(Duration::from_secs(10))
    );
    let error = result.unwrap_err();
    assert!(
        matches!(&error, SignInError::MissingScope(granted) if !granted.contains("chatgpt.tokens.use.direct")),
        "{error:?}"
    );
}

#[tokio::test]
async fn without_a_refresh_token_the_sign_in_is_refused() {
    let openai = Openai::start().await;
    let pending = openai.flow().start(None, 0).await.unwrap();
    let url = pending.authorization_url().to_owned();
    let mut body = tokens(&id_token_for(&url, |_| {}));
    body.as_object_mut().unwrap().remove("refresh_token");
    openai.answers(200, &body).await;
    let params = good_callback(&pending);
    let (_, result) = tokio::join!(
        browser(&pending, &params),
        pending.finish(Duration::from_secs(10))
    );
    assert!(
        matches!(result, Err(SignInError::NoRefreshToken)),
        "{result:?}"
    );
}

/// A sign-in whose ID token is made by `make` from the authorization URL.
async fn with_id_token(make: impl FnOnce(&str) -> String) -> Result<Credentials, SignInError> {
    let openai = Openai::start().await;
    let pending = openai.flow().start(None, 0).await.unwrap();
    let token = make(pending.authorization_url());
    openai.answers(200, &tokens(&token)).await;
    let params = good_callback(&pending);
    let (_, result) = tokio::join!(
        browser(&pending, &params),
        pending.finish(Duration::from_secs(10))
    );
    result
}

fn refused_because(result: Result<Credentials, SignInError>, why: &str) {
    match result {
        Err(SignInError::IdToken(reason)) => {
            assert!(reason.contains(why), "{reason:?} should say {why:?}");
        }
        other => panic!("expected an ID token refusal ({why}), got {other:?}"),
    }
}

#[tokio::test]
async fn an_id_token_is_checked_against_openais_key_and_this_attempt() {
    // Good.
    assert!(with_id_token(|url| id_token_for(url, |_| {})).await.is_ok());
    // The audience can be a list that holds the client.
    assert!(
        with_id_token(|url| id_token_for(url, |c| c["aud"] = json!(["other", ISSUED])))
            .await
            .is_ok()
    );

    refused_because(
        with_id_token(|url| id_token_for(url, |c| c["nonce"] = json!("another-attempt"))).await,
        "nonce",
    );
    refused_because(
        with_id_token(|url| id_token_for(url, |c| c["aud"] = json!("oaiapp_other"))).await,
        "another client",
    );
    refused_because(
        with_id_token(|url| id_token_for(url, |c| c["iss"] = json!("https://evil.example"))).await,
        "not issued by OpenAI",
    );
    refused_because(
        with_id_token(|url| id_token_for(url, |c| c["exp"] = json!(NOW - 3600))).await,
        "expired",
    );
    refused_because(
        with_id_token(|url| {
            id_token_for(url, |c| {
                c.as_object_mut().unwrap().remove("sub");
            })
        })
        .await,
        "subject",
    );
    // A claim changed after signing.
    refused_because(
        with_id_token(|url| {
            let token = id_token_for(url, |_| {});
            let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
            let mut claims: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&parts[1]).unwrap()).unwrap();
            claims["email"] = json!("forged@example.com");
            parts[1] = URL_SAFE_NO_PAD.encode(claims.to_string());
            parts.join(".")
        })
        .await,
        "signature",
    );
    // A key the set does not hold.
    refused_because(
        with_id_token(|url| {
            let query = query_of(url);
            let claims = json!({
                "iss": "https://auth.openai.com", "aud": ISSUED, "sub": "u",
                "nonce": query["nonce"], "exp": NOW + 3600,
            });
            sign(&json!({"alg": "RS256", "kid": "another-key"}), &claims)
        })
        .await,
        "no key",
    );
    // Other algorithms, `none` among them.
    for alg in ["none", "HS256"] {
        refused_because(
            with_id_token(|url| {
                let query = query_of(url);
                let claims = json!({
                    "iss": "https://auth.openai.com", "aud": ISSUED, "sub": "u",
                    "nonce": query["nonce"], "exp": NOW + 3600,
                });
                format!(
                    "{}.{}.",
                    URL_SAFE_NO_PAD.encode(json!({"alg": alg}).to_string()),
                    URL_SAFE_NO_PAD.encode(claims.to_string())
                )
            })
            .await,
            "RS256",
        );
    }
    refused_because(with_id_token(|_| "not-a-jwt".to_owned()).await, "not a JWT");
}

#[tokio::test]
async fn the_state_of_one_sign_in_is_no_good_for_the_next() {
    let openai = Openai::start().await;
    let first = openai.flow().start(None, 0).await.unwrap();
    let second = openai.flow().start(None, 0).await.unwrap();
    let (a, b) = (
        query_of(first.authorization_url()),
        query_of(second.authorization_url()),
    );
    assert_ne!(a["state"], b["state"]);
    assert_ne!(a["nonce"], b["nonce"]);
    assert_ne!(a["code_challenge"], b["code_challenge"]);
    assert_ne!(a["ext_agent_host_id"], b["ext_agent_host_id"]);
    // The pending sign-in's Debug text carries none of them.
    let text = format!("{first:?}");
    assert!(
        !text.contains(&a["state"]) && !text.contains(&a["code_challenge"]),
        "{text}"
    );
}
