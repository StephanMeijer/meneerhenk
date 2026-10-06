//! Signed cookies: the sign-in session and the OAuth state. A value is
//! `base64(payload).base64(hmac)`; tampering or expiry makes it nothing.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;

/// The session cookie's name.
pub const SESSION_COOKIE: &str = "henk_session";
/// The OAuth state cookie's name.
pub const STATE_COOKIE: &str = "henk_oauth";

/// Signs and checks cookie values with one key.
pub struct Signer {
    key: hmac::Key,
    secure: bool,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer").finish_non_exhaustive()
    }
}

/// Who is signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// GitHub user id: what decides access (§2).
    pub github_id: u64,
    /// GitHub login, for display only.
    pub login: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Signer {
    /// A signer over `key`. `secure` adds the `Secure` flag to cookies,
    /// for a dashboard served over https.
    #[must_use]
    pub fn new(key: &[u8], secure: bool) -> Self {
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, key),
            secure,
        }
    }

    /// `payload` with an expiry, signed.
    fn sign(&self, payload: &str, ttl: Duration) -> String {
        let body = format!("{}|{payload}", now() + ttl.as_secs());
        let tag = hmac::sign(&self.key, body.as_bytes());
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(body),
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        )
    }

    /// The payload of a value this signer made, unless forged or expired.
    fn open(&self, value: &str) -> Option<String> {
        let (body, tag) = value.split_once('.')?;
        let body = URL_SAFE_NO_PAD.decode(body).ok()?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        hmac::verify(&self.key, &body, &tag).ok()?;
        let body = String::from_utf8(body).ok()?;
        let (expires, payload) = body.split_once('|')?;
        (expires.parse::<u64>().ok()? > now()).then(|| payload.to_owned())
    }

    /// A `Set-Cookie` value. `Path=/dashboard`: the cookie goes nowhere else.
    #[must_use]
    pub fn cookie(&self, name: &str, value: &str, ttl: Duration) -> String {
        let secure = if self.secure { "; Secure" } else { "" };
        format!(
            "{name}={value}; Path=/dashboard; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
            ttl.as_secs()
        )
    }

    /// A `Set-Cookie` value that removes `name`.
    #[must_use]
    pub fn clear(&self, name: &str) -> String {
        self.cookie(name, "", Duration::ZERO)
    }

    /// The session cookie value for `session`.
    #[must_use]
    pub fn session(&self, session: &Session, ttl: Duration) -> String {
        self.sign(&format!("{}|{}", session.github_id, session.login), ttl)
    }

    /// The session a cookie value carries, if it is genuine and current.
    #[must_use]
    pub fn read_session(&self, value: &str) -> Option<Session> {
        let payload = self.open(value)?;
        let (id, login) = payload.split_once('|')?;
        Some(Session {
            github_id: id.parse().ok()?,
            login: login.to_owned(),
        })
    }

    /// The signed OAuth state value for `state`, valid for ten minutes.
    #[must_use]
    pub fn state(&self, state: &str) -> String {
        self.sign(state, Duration::from_mins(10))
    }

    /// The state an OAuth state cookie carries, if genuine and current.
    #[must_use]
    pub fn read_state(&self, value: &str) -> Option<String> {
        self.open(value)
    }
}

/// The value of cookie `name` in a `Cookie` header.
#[must_use]
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn session() -> Session {
        Session {
            github_id: 1234,
            login: "alice".to_owned(),
        }
    }

    #[test]
    fn a_session_survives_its_own_signer_only() {
        let signer = Signer::new(&[7; 32], true);
        let value = signer.session(&session(), Duration::from_hours(1));
        assert_eq!(signer.read_session(&value), Some(session()));
        assert_eq!(
            Signer::new(&[8; 32], true).read_session(&value),
            None,
            "another key"
        );
        let (body, tag) = value.split_once('.').unwrap();
        let forged_body = URL_SAFE_NO_PAD.encode(
            String::from_utf8(URL_SAFE_NO_PAD.decode(body).unwrap())
                .unwrap()
                .replace("1234", "9999"),
        );
        assert_eq!(
            signer.read_session(&format!("{forged_body}.{tag}")),
            None,
            "a changed id"
        );
        assert_eq!(signer.read_session("nonsense"), None);
        assert_eq!(signer.read_session(""), None);
    }

    #[test]
    fn an_expired_session_is_nothing() {
        let signer = Signer::new(&[7; 32], false);
        let value = signer.session(&session(), Duration::ZERO);
        assert_eq!(signer.read_session(&value), None);
    }

    #[test]
    fn cookies_are_scoped_and_flagged() {
        let secure =
            Signer::new(&[7; 32], true).cookie(SESSION_COOKIE, "v", Duration::from_hours(12));
        assert_eq!(
            secure,
            "henk_session=v; Path=/dashboard; HttpOnly; SameSite=Lax; Max-Age=43200; Secure"
        );
        let plain = Signer::new(&[7; 32], false).clear(STATE_COOKIE);
        assert_eq!(
            plain,
            "henk_oauth=; Path=/dashboard; HttpOnly; SameSite=Lax; Max-Age=0"
        );
        assert_eq!(
            cookie_value("a=1; henk_session=xyz; b=2", SESSION_COOKIE),
            Some("xyz")
        );
        assert_eq!(cookie_value("henk_sessionx=1", SESSION_COOKIE), None);
    }
}
