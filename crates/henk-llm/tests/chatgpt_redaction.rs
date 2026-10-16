//! A token is in no error, no `Debug` text and no log line (#620). This test
//! is a file of its own: `tracing` remembers per call site whether anyone
//! listens, and tests that run beside this one would decide that for it.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::{Dir, NOW, credentials, manager, read, renewal};
use henk_llm::TokenSource;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A `tracing` writer that keeps what was logged.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn a_token_is_in_no_error_no_debug_text_and_no_log_line() {
    let log = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(log.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let secrets = [
        "access-1",
        "refresh-1",
        "id-token-1",
        "access-2",
        "refresh-2",
    ];

    let server = MockServer::start().await;
    // A refusal that quotes the tokens it was sent.
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("bad request: refresh-1 with access-1 and id-token-1"),
        )
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

    let error = manager.bearer().await.unwrap_err();
    let renewed = manager.bearer().await.unwrap();
    let mut seen = vec![
        format!("{error}"),
        format!("{error:?}"),
        format!("{original:?}"),
        format!("{manager:?}"),
        format!("{renewed:?}"),
        format!("{:?}", read(&dir.file())),
    ];
    seen.push(String::from_utf8(log.0.lock().unwrap().clone()).unwrap());

    assert!(!seen[6].is_empty(), "something was logged");
    for text in &seen {
        for secret in secrets {
            assert!(!text.contains(secret), "{secret} in: {text}");
        }
    }
}
