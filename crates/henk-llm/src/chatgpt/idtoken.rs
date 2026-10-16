//! The ID token of a ChatGPT sign-in, checked as OpenAI's docs ask (#620):
//! the signature against OpenAI's published keys, then the issuer, the
//! audience (the client id OpenAI issued), the nonce of this attempt and the
//! expiry.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rsa::PublicKeyComponents;
use ring::signature::RSA_PKCS1_2048_8192_SHA256;
use serde_json::Value;

/// How far past `exp` a token is still taken, for clocks that differ.
const LEEWAY: i64 = 60;

/// What the token must say.
pub(super) struct Expected<'a> {
    /// The issuer.
    pub issuer: &'a str,
    /// The client id OpenAI issued.
    pub audience: &'a str,
    /// The nonce this attempt sent.
    pub nonce: &'a str,
    /// Now, in seconds since the epoch.
    pub now: i64,
}

/// Who the token names.
#[derive(Debug)]
pub(super) struct Identity {
    /// The stable subject.
    pub subject: String,
    /// The email, when the token has one.
    pub email: Option<String>,
}

/// Checks `token` against `jwks` (the JSON of OpenAI's key set) and
/// `expected`. The reasons never quote the token.
pub(super) fn verify(
    token: &str,
    jwks: &Value,
    expected: &Expected<'_>,
) -> Result<Identity, String> {
    let mut parts = token.split('.');
    let (Some(header_part), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("it is not a JWT".to_owned());
    };
    let header: Value = serde_json::from_slice(&decode(header_part, "its header")?)
        .map_err(|_| "its header is not JSON".to_owned())?;
    let algorithm = header.get("alg").and_then(Value::as_str).unwrap_or("");
    if algorithm != "RS256" {
        return Err("it is not signed with RS256, the only algorithm accepted".to_owned());
    }
    let signing_input = format!("{header_part}.{payload}");
    let key = signing_key(jwks, header.get("kid").and_then(Value::as_str))?;
    key.verify(
        &RSA_PKCS1_2048_8192_SHA256,
        signing_input.as_bytes(),
        &decode(signature, "its signature")?,
    )
    .map_err(|_| "its signature does not match OpenAI's key".to_owned())?;

    let claims: Value = serde_json::from_slice(&decode(payload, "its claims")?)
        .map_err(|_| "its claims are not JSON".to_owned())?;
    if claims.get("iss").and_then(Value::as_str) != Some(expected.issuer) {
        return Err("it was not issued by OpenAI".to_owned());
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(aud)) => aud == expected.audience,
        Some(Value::Array(all)) => all
            .iter()
            .any(|aud| aud.as_str() == Some(expected.audience)),
        _ => false,
    };
    if !audience_ok {
        return Err("it was issued for another client".to_owned());
    }
    match claims.get("exp").and_then(Value::as_i64) {
        Some(exp) if exp.saturating_add(LEEWAY) >= expected.now => {}
        Some(_) => return Err("it has expired".to_owned()),
        None => return Err("it has no expiry".to_owned()),
    }
    if claims.get("nonce").and_then(Value::as_str) != Some(expected.nonce) {
        return Err("its nonce is not the one this sign-in sent".to_owned());
    }
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|sub| !sub.is_empty())
        .ok_or_else(|| "it names no subject".to_owned())?
        .to_owned();
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| !email.is_empty())
        .map(str::to_owned);
    Ok(Identity { subject, email })
}

/// The RSA key of `jwks` that signed: the one named `kid`, or the only one
/// when the token names none.
fn signing_key(jwks: &Value, kid: Option<&str>) -> Result<PublicKeyComponents<Vec<u8>>, String> {
    let keys: Vec<&Value> = jwks
        .get("keys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|key| key.get("kty").and_then(Value::as_str) == Some("RSA"))
        .collect();
    let key = match kid {
        Some(kid) => keys
            .iter()
            .find(|key| key.get("kid").and_then(Value::as_str) == Some(kid)),
        None if keys.len() == 1 => keys.first(),
        None => None,
    }
    .ok_or_else(|| "OpenAI's key set has no key for it".to_owned())?;
    let part = |name: &str| {
        key.get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("a key of OpenAI's key set has no {name}"))
            .and_then(|text| decode(text, name))
    };
    Ok(PublicKeyComponents {
        n: part("n")?,
        e: part("e")?,
    })
}

fn decode(text: &str, what: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| format!("{what} is not base64url"))
}
