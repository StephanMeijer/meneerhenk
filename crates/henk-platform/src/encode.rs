//! Percent-encoding for the parts of a URL Henk builds by hand.

use std::fmt::Write as _;

/// `value` percent-encoded: every byte but an unreserved one (`A-Z`, `a-z`,
/// `0-9`, `-`, `.`, `_`, `~`) becomes `%XX`, so `/`, `+`, `&`, `%`, `#` and
/// a space survive the trip.
#[must_use]
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
