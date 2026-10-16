//! The credential file: what a sign-in leaves for the token manager.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};

/// The file's format. A file of another version is not read.
const VERSION: u32 = 1;

/// A ChatGPT sign-in: the client OpenAI issued for this user and workspace,
/// the host that holds it, and the current token pair.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The client id OpenAI issued at registration, bound to the user and
    /// the workspace chosen then.
    pub client_id: String,
    /// The stable id of the host that owns this sign-in. Each Henk process
    /// has its own file and id; a file is never shared between replicas.
    pub ext_agent_host_id: String,
    /// The bearer for the Responses API.
    pub access_token: SecretString,
    /// The token that renews it. Replaced by every renewal.
    pub refresh_token: SecretString,
    /// The ID token from the sign-in or the latest renewal, for the account
    /// it names.
    pub id_token: Option<SecretString>,
    /// When the access token stops working, in seconds since the epoch.
    pub expires_at: i64,
    /// The earliest moment OpenAI wants a renewal, when it said, in seconds
    /// since the epoch.
    pub earliest_refresh_at: Option<i64>,
    /// When the token pair was last issued, in seconds since the epoch.
    pub refreshed_at: i64,
    /// The scopes OpenAI granted.
    pub scopes: Vec<String>,
    /// Who signed in: the account's email or its stable id. Not a secret.
    pub account: Option<String>,
}

/// The file as JSON. Tokens are plain strings only here, in the one place
/// that reads and writes them.
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    client_id: String,
    ext_agent_host_id: String,
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    expires_at: i64,
    #[serde(default)]
    earliest_refresh_at: Option<i64>,
    refreshed_at: i64,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    account: Option<String>,
}

impl Credentials {
    /// The file's content.
    ///
    /// # Errors
    ///
    /// Returns why the credentials cannot be written as JSON.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&Stored {
            version: VERSION,
            client_id: self.client_id.clone(),
            ext_agent_host_id: self.ext_agent_host_id.clone(),
            access_token: self.access_token.expose_secret().to_owned(),
            refresh_token: self.refresh_token.expose_secret().to_owned(),
            id_token: self.id_token.as_ref().map(|t| t.expose_secret().to_owned()),
            expires_at: self.expires_at,
            earliest_refresh_at: self.earliest_refresh_at,
            refreshed_at: self.refreshed_at,
            scopes: self.scopes.clone(),
            account: self.account.clone(),
        })
        .map_err(|error| error.to_string())
    }

    /// Credentials from a file's content.
    ///
    /// # Errors
    ///
    /// Returns why the text is not a credential file this version reads. The
    /// reason never quotes the text.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let stored: Stored = serde_json::from_str(text)
            .map_err(|error| format!("not a sign-in file ({})", error.classify_name()))?;
        if stored.version != VERSION {
            return Err(format!(
                "a sign-in file of version {}, and this Henk reads version {VERSION}",
                stored.version
            ));
        }
        if stored.client_id.is_empty() || stored.refresh_token.is_empty() {
            return Err("a sign-in file without a client id or a refresh token".to_owned());
        }
        Ok(Self {
            client_id: stored.client_id,
            ext_agent_host_id: stored.ext_agent_host_id,
            access_token: stored.access_token.into(),
            refresh_token: stored.refresh_token.into(),
            id_token: stored.id_token.map(Into::into),
            expires_at: stored.expires_at,
            earliest_refresh_at: stored.earliest_refresh_at,
            refreshed_at: stored.refreshed_at,
            scopes: stored.scopes,
            account: stored.account,
        })
    }
}

/// A word for what is wrong with unreadable JSON, without the JSON.
trait ClassifyName {
    fn classify_name(&self) -> &'static str;
}

impl ClassifyName for serde_json::Error {
    fn classify_name(&self) -> &'static str {
        match self.classify() {
            serde_json::error::Category::Io => "unreadable",
            serde_json::error::Category::Syntax => "not JSON",
            serde_json::error::Category::Data => "not the expected fields",
            serde_json::error::Category::Eof => "cut short",
        }
    }
}

/// Writes `content` to `path` so that a reader sees the old file or the new
/// one, never part of either: a file of its own beside it, owner-only, on
/// disk, then renamed over `path`.
///
/// # Errors
///
/// Returns the I/O error.
pub(super) fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    write_atomic_through(path, content, &temporary_path(path, serial))
}

/// The name of the file a write goes through: unique to the process and to
/// the write.
fn temporary_path(path: &Path, serial: u64) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "credentials".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(".{name}.{}.{serial}.tmp", std::process::id()))
}

/// Creates `temporary` new, owner-only. A file already there (left by a
/// crash, or put there by someone else) is removed first, never reused: it
/// would keep its own owner and permissions.
fn create_new_private(temporary: &Path) -> std::io::Result<std::fs::File> {
    let open = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temporary)
    };
    match open() {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(temporary)?;
            open()
        }
        other => other,
    }
}

fn write_atomic_through(path: &Path, content: &str, temporary: &Path) -> std::io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let result = (|| {
        let mut file = create_new_private(temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
        return result;
    }
    if let Some(parent) = parent
        && let Ok(directory) = std::fs::File::open(parent)
    {
        // Best effort: the rename itself is on disk too.
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_stale_temporary_file_is_replaced_not_reused() {
        let dir = std::env::temp_dir().join(format!("henk-credentials-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("openai.json");
        let temporary = temporary_path(&path, 7);
        std::fs::write(&temporary, "stale").unwrap();
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_atomic_through(&path, "fresh", &temporary).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "owner only, whatever the stale file was"
        );
        assert!(!temporary.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_writes_do_not_share_a_temporary_name() {
        let path = Path::new("/tmp/openai.json");
        assert_ne!(temporary_path(path, 1), temporary_path(path, 2));
    }
}
