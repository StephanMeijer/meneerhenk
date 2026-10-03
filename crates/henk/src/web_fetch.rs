//! `web_fetch`: read a public web page as text. The request carries nothing
//! but the URL (§11, no side door), and private addresses are refused.

use std::time::Duration;

use henk_agent::{Tool, ToolOutput};
use henk_llm::{ToolDef, ToolName};
use serde_json::{Value, json};

const MAX_BYTES: usize = 200 * 1024;
const MAX_URL: usize = 512;

/// The tool.
#[derive(Debug)]
pub struct WebFetch {
    http: reqwest::Client,
}

impl WebFetch {
    /// Builds the tool with its own client: no cookies, no credentials.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub fn new() -> anyhow::Result<Self> {
        henk_llm::ensure_tls_provider();
        let http = reqwest::Client::builder()
            .user_agent("meneer-henk (planning; reads documentation)")
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()?;
        Ok(Self { http })
    }
}

/// Why a URL is refused.
fn refuse(url: &str) -> Option<&'static str> {
    if url.len() > MAX_URL {
        return Some("URL is too long");
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return Some("only https URLs are fetched");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Some("URLs with credentials are not fetched");
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(h, _)| h)
        .trim_matches(['[', ']']);
    let lower = host.to_ascii_lowercase();
    let last_label = lower.rsplit('.').next().unwrap_or("");
    let private = matches!(last_label, "localhost" | "local" | "internal")
        || lower == "::1"
        || lower.starts_with("127.")
        || lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("169.254.")
        || lower.starts_with("0.")
        || lower.starts_with("fe80:")
        || lower.starts_with("fc")
        || lower.starts_with("fd")
        || (lower.starts_with("172.")
            && lower
                .split('.')
                .nth(1)
                .and_then(|o| o.parse::<u8>().ok())
                .is_some_and(|o| (16..=31).contains(&o)));
    if private || lower.is_empty() {
        return Some("private or local addresses are not fetched");
    }
    None
}

/// Strips scripts, styles and tags; collapses whitespace.
fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    let mut in_tag = false;
    let mut skip_until: Option<&str> = None;
    while !rest.is_empty() {
        if let Some(end_tag) = skip_until {
            match rest.to_ascii_lowercase().find(end_tag) {
                Some(at) => {
                    rest = rest.get(at + end_tag.len()..).unwrap_or("");
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        let Some(c) = rest.chars().next() else { break };
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            rest = rest.get(c.len_utf8()..).unwrap_or("");
            continue;
        }
        if c == '<' {
            let lower = rest
                .get(..rest.len().min(8))
                .unwrap_or("")
                .to_ascii_lowercase();
            if lower.starts_with("<script") {
                skip_until = Some("</script>");
            } else if lower.starts_with("<style") {
                skip_until = Some("</style>");
            } else {
                in_tag = true;
            }
            rest = rest.get(1..).unwrap_or("");
            out.push(' ');
            continue;
        }
        out.push(c);
        rest = rest.get(c.len_utf8()..).unwrap_or("");
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::with_capacity(decoded.len());
    let mut last_space = false;
    let mut newlines = 0;
    for c in decoded.chars() {
        if c == '\n' {
            newlines += 1;
            while collapsed.ends_with(' ') {
                collapsed.pop();
            }
            if newlines <= 2 {
                collapsed.push('\n');
            }
            last_space = true;
        } else if c.is_whitespace() {
            if !last_space {
                collapsed.push(' ');
            }
            last_space = true;
        } else {
            collapsed.push(c);
            last_space = false;
            newlines = 0;
        }
    }
    collapsed.trim().to_owned()
}

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("web_fetch").unwrap_or_else(|_| unreachable!("constant")),
            description: "Fetches a public https web page and returns its text, for documentation. Nothing but the URL is sent.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"url": {"type": "string", "description": "An https URL"}},
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, arguments: Value) -> ToolOutput {
        let Some(url) = arguments.get("url").and_then(Value::as_str).map(str::trim) else {
            return ToolOutput::error("url is required");
        };
        if let Some(reason) = refuse(url) {
            return ToolOutput::error(format!("Refused: {reason}"));
        }
        let response = match self.http.get(url).send().await {
            Ok(response) => response,
            Err(error) => return ToolOutput::error(format!("Fetch failed: {error}")),
        };
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return ToolOutput::error(format!("Fetch failed while reading: {error}")),
        };
        let body =
            String::from_utf8_lossy(bytes.get(..bytes.len().min(MAX_BYTES)).unwrap_or(&bytes))
                .into_owned();
        if !status.is_success() {
            return ToolOutput::error(format!("HTTP {status}"));
        }
        let text = if content_type.contains("html") {
            to_text(&body)
        } else {
            body
        };
        let truncated = if bytes.len() > MAX_BYTES {
            "\n\n[truncated]"
        } else {
            ""
        };
        ToolOutput::ok(format!("{text}{truncated}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_non_https_urls_are_refused() {
        assert!(refuse("http://example.com").is_some());
        assert!(refuse("https://localhost/x").is_some());
        assert!(refuse("https://127.0.0.1/").is_some());
        assert!(refuse("https://10.1.2.3/").is_some());
        assert!(refuse("https://172.20.0.1/").is_some());
        assert!(refuse("https://192.168.1.1/").is_some());
        assert!(refuse("https://user:pw@example.com/").is_some());
        assert!(refuse("https://metadata.internal/").is_some());
        assert!(refuse("https://docs.rs/tokio").is_none());
        assert!(
            refuse("https://172.32.0.1/").is_none(),
            "outside the private block"
        );
    }

    #[test]
    fn html_becomes_text() {
        let html = "<html><head><style>p{}</style><script>x()</script></head><body><h1>Title</h1>\n<p>Hello &amp; welcome</p></body></html>";
        assert_eq!(
            to_text(html),
            "Title\n Hello & welcome".replace("\n ", "\n")
        );
    }
}
