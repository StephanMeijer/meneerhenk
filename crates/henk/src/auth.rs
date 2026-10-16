//! `henk auth openai`: sign an endpoint in with a ChatGPT plan (#620).
//!
//! The sign-in itself is `henk_llm::chatgpt::SignIn`; this is the command
//! around it: which endpoint, the file it is kept in, the browser, and the
//! words printed. No token is ever printed.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_llm::chatgpt::{Credentials, LoadError, SignIn};

use crate::config::Settings;

/// How long the browser has to come back.
const WAIT: Duration = Duration::from_mins(5);

/// The ChatGPT endpoint to sign in: the one named, or the only one there is.
///
/// # Errors
///
/// Returns what to fix: no ChatGPT endpoint, several and none named, or the
/// named one is not a ChatGPT endpoint.
pub fn endpoint_to_sign_in<'a>(
    settings: &'a Settings,
    named: Option<&'a str>,
) -> Result<(&'a str, &'a Path), String> {
    let chatgpt: Vec<(&str, &Path)> = settings
        .endpoints
        .iter()
        .filter(|(_, endpoint)| endpoint.signs_in_with_chatgpt())
        .filter_map(|(id, endpoint)| Some((id.as_str(), endpoint.sign_in_file()?)))
        .collect();
    match named {
        Some(id) => chatgpt
            .iter()
            .copied()
            .find(|(candidate, _)| *candidate == id)
            .ok_or_else(|| {
                if settings.endpoints.contains_key(id) {
                    format!(
                        "endpoint {id} pays with an API key; to sign it in with ChatGPT, give it auth = \"chatgpt\" and an auth_file instead of api_key_env"
                    )
                } else {
                    format!("endpoint {id} is not configured")
                }
            }),
        None => match chatgpt.as_slice() {
            [] => Err("no endpoint has auth = \"chatgpt\"; add one with an auth_file (see henk.example.toml)".to_owned()),
            [one] => Ok(*one),
            many => Err(format!(
                "several endpoints sign in with ChatGPT ({}); name one with --endpoint",
                many.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(", ")
            )),
        },
    }
}

/// The sign-in already in `file`, to renew with the same client and host.
///
/// # Errors
///
/// Returns an error for a file that exists and is not a sign-in file: it is
/// not written over.
pub fn existing_sign_in(file: &Path) -> anyhow::Result<Option<Credentials>> {
    match Credentials::load(file) {
        Ok(credentials) => Ok(Some(credentials)),
        Err(LoadError::Missing(_)) => Ok(None),
        Err(error @ LoadError::Unusable { .. }) => Err(anyhow!(
            "{error}; remove it to sign in afresh, or point auth_file somewhere else"
        )),
    }
}

/// Signs `endpoint` in: starts the flow, waits for the browser, saves the
/// file.
///
/// # Errors
///
/// Returns why the sign-in did not complete or the file could not be written.
pub async fn openai(
    settings: &Settings,
    endpoint: Option<&str>,
    port: u16,
    no_browser: bool,
) -> anyhow::Result<()> {
    let (id, file) = endpoint_to_sign_in(settings, endpoint).map_err(|why| anyhow!("{why}"))?;
    let existing = existing_sign_in(file)?;
    let again = existing.is_some();
    henk_llm::ensure_tls_provider();
    let http = henk_llm::build_endpoint_client()?;
    let pending = SignIn::new(http).start(existing, port).await?;

    eprintln!(
        "Signing endpoint {id} in with ChatGPT{}.",
        if again { " again" } else { "" }
    );
    eprintln!("Open this address in a browser and sign in to ChatGPT:\n");
    eprintln!("  {}\n", pending.authorization_url());
    eprintln!(
        "Waiting up to {} minutes for the browser to come back to {}.",
        WAIT.as_secs() / 60,
        pending.redirect_uri()
    );
    eprintln!(
        "On a server without a browser: forward the port from your own machine with\n  ssh -L {port}:127.0.0.1:{port} <server>\nand open the address there, or run this on a machine with a browser and copy\n{} to the server.",
        file.display(),
        port = redirect_port(pending.redirect_uri())
    );
    if !no_browser {
        open_browser(pending.authorization_url());
    }

    let credentials = tokio::select! {
        done = pending.finish(WAIT) => done?,
        _ = tokio::signal::ctrl_c() => return Err(anyhow!("cancelled")),
    };
    credentials
        .save(file)
        .with_context(|| format!("writing {}", file.display()))?;
    println!(
        "Signed in as {}. The sign-in is in {} (owner-only); Henk renews it there.",
        credentials.account.as_deref().unwrap_or("an account"),
        file.display()
    );
    Ok(())
}

/// The port of a `http://127.0.0.1:PORT/...` address.
fn redirect_port(redirect_uri: &str) -> &str {
    redirect_uri
        .rsplit(':')
        .next()
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
}

/// Asks the desktop to open `url`. A machine without one prints the address
/// anyway, so a failure here is not worth more than silence.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn settings(endpoints: &str) -> Settings {
        let text = format!(
            "[discord]\nchannel_id = 1\nhenk_user_id = 2\nteam_lead_ids = [3]\n[mail]\naddress = \"henk@example.com\"\n[allowlist]\ngithub_owners = [\"docspec\"]\n{endpoints}"
        );
        crate::config::Config::parse(&text)
            .and_then(crate::config::Config::into_settings)
            .unwrap()
    }

    #[test]
    fn the_only_chatgpt_endpoint_is_chosen_without_a_name() {
        let settings = settings(
            "[endpoints.c]\nprovider = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\nauth = \"chatgpt\"\nauth_file = \"/f.json\"\n[endpoints.k]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\n",
        );
        let (id, file) = endpoint_to_sign_in(&settings, None).unwrap();
        assert_eq!((id, file), ("c", Path::new("/f.json")));
        assert_eq!(endpoint_to_sign_in(&settings, Some("c")).unwrap().0, "c");
        let key = endpoint_to_sign_in(&settings, Some("k")).unwrap_err();
        assert!(key.contains("pays with an API key"), "{key}");
        let unknown = endpoint_to_sign_in(&settings, Some("z")).unwrap_err();
        assert!(unknown.contains("not configured"), "{unknown}");
    }

    #[test]
    fn none_or_several_need_saying() {
        let none = settings(
            "[endpoints.k]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"K\"\n",
        );
        assert!(
            endpoint_to_sign_in(&none, None)
                .unwrap_err()
                .contains("no endpoint has auth")
        );
        let two = settings(
            "[endpoints.a]\nprovider = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\nauth = \"chatgpt\"\nauth_file = \"/a.json\"\n[endpoints.b]\nprovider = \"openai\"\nbase_url = \"https://api.openai.com/v1\"\nauth = \"chatgpt\"\nauth_file = \"/b.json\"\n",
        );
        let said = endpoint_to_sign_in(&two, None).unwrap_err();
        assert!(
            said.contains("a, b") && said.contains("--endpoint"),
            "{said}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_sign_in_is_not_written_over() {
        let dir = std::env::temp_dir().join(format!("henk-auth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("openai.json");
        assert!(existing_sign_in(&file).unwrap().is_none());
        std::fs::write(&file, "not a sign-in file at all").unwrap();
        let said = existing_sign_in(&file).unwrap_err().to_string();
        assert!(said.contains("remove it"), "{said}");
        assert!(
            !said.contains("not a sign-in file at all"),
            "the file's text is not quoted: {said}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_port_is_read_from_the_redirect_address() {
        assert_eq!(redirect_port("http://127.0.0.1:1455/auth/callback"), "1455");
    }
}
