//! Webhook authentication.

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

/// Checks GitHub's `X-Hub-Signature-256` header against the raw body.
#[must_use]
pub fn verify_github_signature(secret: &[u8], header: Option<&str>, body: &[u8]) -> bool {
    let Some(hex) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Some(given) = decode_hex(hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&given).is_ok()
}

/// Checks GitLab's `X-Gitlab-Token` header in constant time.
#[must_use]
pub fn verify_gitlab_token(expected: &str, header: Option<&str>) -> bool {
    let Some(given) = header else {
        return false;
    };
    expected.as_bytes().ct_eq(given.as_bytes()).into()
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // HMAC-SHA256("secret", "hello") per RFC 2104 test tooling.
    const HELLO_SIG: &str =
        "sha256=88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b";

    #[test]
    fn github_signature_matches() {
        assert!(verify_github_signature(
            b"secret",
            Some(HELLO_SIG),
            b"hello"
        ));
        assert!(!verify_github_signature(
            b"secret",
            Some(HELLO_SIG),
            b"hello!"
        ));
        assert!(!verify_github_signature(
            b"other",
            Some(HELLO_SIG),
            b"hello"
        ));
        assert!(!verify_github_signature(b"secret", None, b"hello"));
        assert!(!verify_github_signature(
            b"secret",
            Some("sha1=abc"),
            b"hello"
        ));
        assert!(!verify_github_signature(
            b"secret",
            Some("sha256=zz"),
            b"hello"
        ));
    }

    #[test]
    fn gitlab_token_compares_exactly() {
        assert!(verify_gitlab_token("tok", Some("tok")));
        assert!(!verify_gitlab_token("tok", Some("tok2")));
        assert!(!verify_gitlab_token("tok", None));
    }
}
