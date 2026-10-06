//! `web_fetch`: read a public web page as text. The request carries nothing
//! but the URL (§11, no side door). Private and local addresses are refused
//! on the parsed host, on every address a name resolves to (the connection
//! uses exactly the addresses checked) and on every redirect. Proxies from
//! the environment are not used, since a proxy would resolve names itself.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use henk_agent::{Tool, ToolOutput};
use henk_llm::{ToolDef, ToolName};
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::{Value, json};

const MAX_BYTES: usize = 200 * 1024;
const MAX_URL: usize = 512;
const MAX_REDIRECTS: usize = 3;
const PRIVATE: &str = "private or local addresses are not fetched";

/// A refusal by one of the guards, carried through reqwest's errors so a
/// refused redirect or name reads the same as a refused URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Refused(&'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Refused {}

/// The tool.
#[derive(Debug)]
pub struct WebFetch {
    http: reqwest::Client,
}

impl WebFetch {
    /// Builds the tool with its own client: no cookies, no credentials, no
    /// proxy, https only, and the address guard on every name and redirect.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub fn new() -> anyhow::Result<Self> {
        Self::build(GuardedResolver::system(), |builder| builder)
    }

    fn build(
        resolver: GuardedResolver,
        configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> anyhow::Result<Self> {
        henk_llm::ensure_tls_provider();
        let builder = reqwest::Client::builder()
            .user_agent("meneer-henk (planning; reads documentation)")
            .timeout(Duration::from_secs(20))
            .https_only(true)
            .no_proxy()
            .dns_resolver(Arc::new(resolver))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                // The first entry is the original URL, not a redirect.
                if attempt.previous().len() > MAX_REDIRECTS {
                    return attempt.error(Refused("too many redirects"));
                }
                match refuse_url(attempt.url().as_str()) {
                    Ok(_) => attempt.follow(),
                    Err(refused) => attempt.error(refused),
                }
            }));
        Ok(Self {
            http: configure(builder).build()?,
        })
    }
}

/// The URL to fetch, or why it is refused. Parsing follows WHATWG, so
/// `2130706433`, `0x7f000001` and `0177.0.0.1` all arrive as `127.0.0.1`.
fn refuse_url(url: &str) -> Result<Url, Refused> {
    if url.len() > MAX_URL {
        return Err(Refused("URL is too long"));
    }
    let parsed = Url::parse(url).map_err(|_| Refused("not a valid URL"))?;
    if parsed.scheme() != "https" {
        return Err(Refused("only https URLs are fetched"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Refused("URLs with credentials are not fetched"));
    }
    let host = parsed.host_str().unwrap_or("");
    let ip = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => v6.parse::<Ipv6Addr>().ok().map(IpAddr::V6),
        None => host.parse::<Ipv4Addr>().ok().map(IpAddr::V4),
    };
    let private = if let Some(ip) = ip {
        forbidden_ip(ip)
    } else {
        let name = host.strip_suffix('.').unwrap_or(host);
        let last_label = name.rsplit('.').next().unwrap_or("");
        name.is_empty() || matches!(last_label, "localhost" | "local" | "internal")
    };
    if private {
        return Err(Refused(PRIVATE));
    }
    Ok(parsed)
}

/// Whether an address is anything but a public unicast one.
fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => forbidden_v4(v4),
        IpAddr::V6(v6) => forbidden_v6(v6),
    }
}

fn forbidden_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking 198.18/15
        || a >= 240 // reserved
}

fn forbidden_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return forbidden_v4(v4);
    }
    let [s0, s1, s2, s3, s4, s5, s6, s7] = ip.segments();
    let ([a, b], [c, d]) = (s6.to_be_bytes(), s7.to_be_bytes());
    let embedded = Ipv4Addr::new(a, b, c, d);
    if s0 == 0x64 && s1 == 0xff9b && s2 == 0 && s3 == 0 && s4 == 0 && s5 == 0 {
        // NAT64 64:ff9b::/96 reaches the IPv4 address it embeds.
        return forbidden_v4(embedded);
    }
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s0 == 0 && s1 == 0 && s2 == 0 && s3 == 0 && s4 == 0 && s5 == 0) // IPv4-compatible
        || (s0 & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (s0 & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (s0 & 0xffc0) == 0xfec0 // site-local fec0::/10
        || (s0 == 0x2001 && s1 == 0x0db8) // documentation
}

/// Name lookup, behind a trait so tests can answer without DNS.
trait Lookup: Send + Sync {
    fn lookup(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>;
}

/// The system resolver, as reqwest's default uses it.
struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), 0)).await?;
            Ok(addrs.map(|addr| addr.ip()).collect())
        })
    }
}

/// Resolves a name and refuses it when any of its addresses is private. A
/// mixed answer is refused whole, since the connection may pick any of
/// them. The addresses returned are the ones checked, so nothing resolves
/// the name again between the check and the connection.
struct GuardedResolver {
    lookup: Arc<dyn Lookup>,
    /// One address the guard lets through; only tests set it, to reach a
    /// server on loopback by name. URLs with an IP host never pass here.
    exempt: Option<IpAddr>,
}

impl GuardedResolver {
    fn system() -> Self {
        Self {
            lookup: Arc::new(SystemLookup),
            exempt: None,
        }
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let lookup = Arc::clone(&self.lookup);
        let exempt = self.exempt;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let ips = lookup.lookup(host).await?;
            if ips.is_empty() {
                return Err(Refused("the name has no address").into());
            }
            if ips
                .iter()
                .any(|ip| Some(*ip) != exempt && forbidden_ip(*ip))
            {
                return Err(Refused(PRIVATE).into());
            }
            let addrs: Addrs = Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// The reason of a [`Refused`] anywhere in an error's source chain.
fn refused_in(error: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(refused) = error.downcast_ref::<Refused>() {
            return Some(refused.0);
        }
        current = error.source();
    }
    None
}

/// The body, at most [`MAX_BYTES`] of it, read as it streams, and whether
/// there was more. Reading stops one byte past the cap and the response is
/// dropped, which closes the connection with the rest unread (#55): no page
/// makes Henk hold more than the cap and one chunk.
async fn read_capped(mut response: reqwest::Response) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = MAX_BYTES + 1 - body.len();
        body.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or(&chunk));
        if body.len() > MAX_BYTES {
            body.truncate(MAX_BYTES);
            return Ok((body, true));
        }
    }
    Ok((body, false))
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
            description: "Fetches a public https web page and returns its text, for documentation. Nothing but the URL is sent. Private and local addresses are refused, also behind a name or a redirect.".to_owned(),
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
        let url = match refuse_url(url) {
            Ok(url) => url,
            Err(Refused(reason)) => return ToolOutput::error(format!("Refused: {reason}")),
        };
        let response = match self.http.get(url).send().await {
            Ok(response) => response,
            Err(error) => {
                return ToolOutput::error(match refused_in(&error) {
                    Some(reason) => format!("Refused: {reason}"),
                    None => format!("Fetch failed: {error}"),
                });
            }
        };
        let status = response.status();
        if !status.is_success() {
            // Before any of the body is read: an error page can be any size.
            return ToolOutput::error(format!("HTTP {status}"));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let (bytes, cut) = match read_capped(response).await {
            Ok(read) => read,
            Err(error) => return ToolOutput::error(format!("Fetch failed while reading: {error}")),
        };
        let body = String::from_utf8_lossy(&bytes).into_owned();
        let text = if content_type.contains("html") {
            to_text(&body)
        } else {
            body
        };
        let truncated = if cut { "\n\n[truncated]" } else { "" };
        ToolOutput::ok(format!("{text}{truncated}"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::collections::HashMap;

    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    const CERT: &[u8] = include_bytes!("../tests/fixtures/web-fetch-test.crt");
    const KEY: &[u8] = include_bytes!("../tests/fixtures/web-fetch-test.key");
    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn refused(url: &str) -> bool {
        refuse_url(url).is_err()
    }

    #[test]
    fn private_and_non_https_urls_are_refused() {
        assert!(refused("http://example.com"));
        assert!(refused("https://localhost/x"));
        assert!(refused("https://127.0.0.1/"));
        assert!(refused("https://10.1.2.3/"));
        assert!(refused("https://172.20.0.1/"));
        assert!(refused("https://192.168.1.1/"));
        assert!(refused("https://user:pw@example.com/"));
        assert!(refused("https://metadata.internal/"));
        assert!(!refused("https://docs.rs/tokio"));
        assert!(!refused("https://172.32.0.1/"), "outside the private block");
    }

    #[test]
    fn refuses_private_hosts_in_every_spelling() {
        for url in [
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://2130706433/",
            "https://0x7f000001/",
            "https://0177.0.0.1/",
            "https://0/",
            "https://100.64.0.1/",
            "https://localhost./",
            "https://loc\talhost/",
            "https://printer.local./",
            "https://[fd00::1]/",
            "https://[fe80::1]/",
            "https://[64:ff9b::7f00:1]/",
            "https://169.254.169.254/latest/meta-data/",
        ] {
            assert_eq!(refuse_url(url), Err(Refused(PRIVATE)), "{url:?}");
        }
        for url in [
            "https://fda.gov/",
            "https://fcc.gov/",
            "https://fd.io/",
            "https://docs.rs/tokio",
            "https://[2606:4700::1111]/",
        ] {
            assert!(refuse_url(url).is_ok(), "{url}");
        }
        assert_eq!(
            refuse_url(&format!("https://example.com/{}", "a".repeat(MAX_URL))),
            Err(Refused("URL is too long"))
        );
    }

    #[test]
    fn forbidden_ips_at_the_edges() {
        for (ip, forbidden) in [
            ("100.63.255.255", false),
            ("100.64.0.0", true),
            ("100.127.255.255", true),
            ("100.128.0.0", false),
            ("172.32.0.1", false),
            ("198.18.0.1", true),
            ("240.0.0.1", true),
            ("8.8.8.8", false),
            ("::ffff:10.0.0.1", true),
            ("::ffff:8.8.8.8", false),
            ("64:ff9b::808:808", false),
            ("fc00::1", true),
            ("fbff::1", false),
            ("2001:db8::1", true),
        ] {
            assert_eq!(forbidden_ip(ip.parse().unwrap()), forbidden, "{ip}");
        }
    }

    /// Answers lookups from a table; any other name is not found.
    struct Stub(HashMap<&'static str, Vec<IpAddr>>);

    impl Lookup for Stub {
        fn lookup(
            &self,
            host: String,
        ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>> {
            let answer = self
                .0
                .get(host.as_str())
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound));
            Box::pin(async move { answer })
        }
    }

    fn fetcher(answers: &[(&'static str, &[IpAddr])], exempt: Option<IpAddr>) -> WebFetch {
        let table = answers
            .iter()
            .map(|(name, ips)| (*name, ips.to_vec()))
            .collect();
        let resolver = GuardedResolver {
            lookup: Arc::new(Stub(table)),
            exempt,
        };
        let root = reqwest::Certificate::from_pem(CERT).unwrap();
        WebFetch::build(resolver, |builder| builder.tls_certs_only([root])).unwrap()
    }

    #[tokio::test]
    async fn a_name_resolving_to_a_private_address_is_refused_before_connecting() {
        let listener = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let fetch = fetcher(&[("intranet.test", &[LOOPBACK])], None);
        let output = fetch
            .call(json!({"url": format!("https://intranet.test:{port}/")}))
            .await;
        assert!(output.is_error);
        assert_eq!(output.content, format!("Refused: {PRIVATE}"));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "no connection was made"
        );

        let mixed = GuardedResolver {
            lookup: Arc::new(Stub(HashMap::from([(
                "mixed.test",
                vec![
                    "93.184.215.14".parse().unwrap(),
                    "10.0.0.1".parse().unwrap(),
                ],
            )]))),
            exempt: None,
        };
        let Err(error) = mixed.resolve("mixed.test".parse().unwrap()).await else {
            panic!("a mixed answer must be refused");
        };
        assert_eq!(refused_in(error.as_ref()), Some(PRIVATE));
    }

    /// What the test server sends for one request.
    enum Reply {
        /// The whole response, as written.
        Fixed(String),
        /// `head`, then `total` bytes of `a` in 64 KiB blocks, until a write
        /// fails because the client went away.
        Stream { head: String, total: usize },
    }

    /// An HTTPS server for `docs.test` on loopback. `answer` gets the port
    /// and the request and says what to send. Returns the port and how many
    /// body bytes a `Stream` reply got written before the client stopped.
    async fn tls_server(
        answer: impl Fn(u16, &[u8]) -> Reply + Send + Sync + 'static,
    ) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let cert = CertificateDer::from_pem_slice(CERT).unwrap();
        let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&written);
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let (answer, counter) = (Arc::clone(&answer), Arc::clone(&counter));
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&chunk[..n]),
                        }
                    }
                    match answer(port, &request) {
                        Reply::Fixed(response) => {
                            let _ = tls.write_all(response.as_bytes()).await;
                        }
                        Reply::Stream { head, total } => {
                            if tls.write_all(head.as_bytes()).await.is_err() {
                                return;
                            }
                            let block = vec![b'a'; 64 * 1024];
                            let mut sent = 0;
                            while sent < total {
                                let n = block.len().min(total - sent);
                                if tls.write_all(&block[..n]).await.is_err() {
                                    break;
                                }
                                sent += n;
                                counter.store(sent, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                    }
                    let _ = tls.shutdown().await;
                });
            }
        });
        (port, written)
    }

    /// An HTTPS server for `docs.test` on loopback. `/` answers with a 302 to
    /// `location(port)`; any other path answers 200 with a secret body.
    async fn redirecting_server(location: impl Fn(u16) -> String + Send + Sync + 'static) -> u16 {
        let (port, _) = tls_server(move |port, request| {
            Reply::Fixed(if request.starts_with(b"GET / ") {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    location(port)
                )
            } else {
                "HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nTARGET-SECRET"
                    .to_owned()
            })
        })
        .await;
        port
    }

    /// Fifty megabytes: far more than any cap or socket buffer.
    const HUGE: usize = 50 * 1024 * 1024;

    /// Well under [`HUGE`]. Loopback socket buffers take a few megabytes after
    /// the client stops reading, so "about 200 KB" is checked as "not the
    /// whole body".
    const STOPPED_EARLY: usize = 16 * 1024 * 1024;

    /// The bytes written once the server's writes stop moving.
    async fn settled(written: &std::sync::atomic::AtomicUsize) -> usize {
        let mut last = usize::MAX;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let now = written.load(std::sync::atomic::Ordering::SeqCst);
            if now == last {
                return now;
            }
            last = now;
        }
        last
    }

    fn streaming(status: &'static str, total: usize) -> impl Fn(u16, &[u8]) -> Reply + Send + Sync {
        move |_, _| Reply::Stream {
            head: format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
            ),
            total,
        }
    }

    #[tokio::test]
    async fn a_large_body_is_cut_while_it_streams() {
        let (port, written) = tls_server(streaming("200 OK", HUGE)).await;
        let fetch = fetcher(&[("docs.test", &[LOOPBACK])], Some(LOOPBACK));
        let output = fetch
            .call(json!({"url": format!("https://docs.test:{port}/big")}))
            .await;
        assert!(
            !output.is_error,
            "{}",
            output.content.get(..200).unwrap_or(&output.content)
        );
        let text = output
            .content
            .strip_suffix("\n\n[truncated]")
            .expect("marked as cut");
        assert_eq!(text.len(), MAX_BYTES);
        assert!(text.bytes().all(|b| b == b'a'));
        let sent = settled(&written).await;
        assert!(
            sent < STOPPED_EARLY,
            "{sent} bytes were sent before the client stopped"
        );
    }

    #[tokio::test]
    async fn an_error_status_is_reported_without_reading_the_body() {
        let (port, written) = tls_server(streaming("500 Internal Server Error", HUGE)).await;
        let fetch = fetcher(&[("docs.test", &[LOOPBACK])], Some(LOOPBACK));
        let output = fetch
            .call(json!({"url": format!("https://docs.test:{port}/broken")}))
            .await;
        assert!(output.is_error);
        assert_eq!(output.content, "HTTP 500 Internal Server Error");
        let sent = settled(&written).await;
        assert!(
            sent < STOPPED_EARLY,
            "{sent} bytes were sent before the client stopped"
        );
    }

    #[tokio::test]
    async fn a_body_at_the_cap_is_not_marked_truncated() {
        for (total, cut) in [(MAX_BYTES, false), (MAX_BYTES + 1, true)] {
            let (port, _) = tls_server(streaming("200 OK", total)).await;
            let fetch = fetcher(&[("docs.test", &[LOOPBACK])], Some(LOOPBACK));
            let output = fetch
                .call(json!({"url": format!("https://docs.test:{port}/edge")}))
                .await;
            assert!(!output.is_error);
            assert_eq!(
                output.content.ends_with("[truncated]"),
                cut,
                "{total} bytes"
            );
            let text = output.content.trim_end_matches("\n\n[truncated]");
            assert_eq!(text.len(), MAX_BYTES, "{total} bytes");
        }
    }

    #[tokio::test]
    async fn redirects_are_checked_at_every_hop() {
        /// Where `/` redirects to, and the refusal expected (empty: any error).
        type Case = (fn(u16) -> String, &'static str);
        let cases: [Case; 3] = [
            (|port| format!("http://127.0.0.1:{port}/secret"), ""),
            (|port| format!("https://127.0.0.1:{port}/secret"), PRIVATE),
            (|port| format!("https://rebind.test:{port}/secret"), PRIVATE),
        ];
        for (location, reason) in cases {
            let port = redirecting_server(location).await;
            let fetch = fetcher(
                &[
                    ("docs.test", &[LOOPBACK]),
                    ("rebind.test", &["10.0.0.1".parse().unwrap()]),
                ],
                Some(LOOPBACK),
            );
            let output = fetch
                .call(json!({"url": format!("https://docs.test:{port}/")}))
                .await;
            let target = location(port);
            assert!(output.is_error, "{target}: {}", output.content);
            assert!(!output.content.contains("TARGET-SECRET"), "{target}");
            if !reason.is_empty() {
                assert_eq!(output.content, format!("Refused: {reason}"), "{target}");
            }
        }
    }

    #[tokio::test]
    async fn the_test_server_is_reachable_without_a_redirect() {
        // Guards the redirect test against passing for the wrong reason: the
        // stub, the exemption and the certificate do let a request through.
        let port = redirecting_server(|_| String::new()).await;
        let fetch = fetcher(&[("docs.test", &[LOOPBACK])], Some(LOOPBACK));
        let output = fetch
            .call(json!({"url": format!("https://docs.test:{port}/page")}))
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "TARGET-SECRET");
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
