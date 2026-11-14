//! OAuth credentials, one per server URL, in a file only the user can read.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// What one server's login produced, and what a refresh needs to renew it.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Credential {
    /// Keys this credential: a renamed server keeps its login.
    pub(crate) server_url: String,
    pub(crate) authorization_url: String,
    pub(crate) token_url: String,
    pub(crate) client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_secret: Option<String>,
    pub(crate) access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    /// Unix seconds; a token at or past it is refreshed before use.
    pub(crate) expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scopes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resource: Option<String>,
}

impl Credential {
    pub(crate) fn expired(&self) -> bool {
        self.expires_at <= now()
    }
}

/// Every secret is printed as this, so an incidental `{:?}` cannot leak a live token.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("server_url", &self.server_url)
            .field("authorization_url", &self.authorization_url)
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| super::REDACTED))
            .field("access_token", &super::REDACTED)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| super::REDACTED))
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .field("resource", &self.resource)
            .finish()
    }
}

/// Seconds since the epoch; a clock before it is treated as 0 rather than as an error.
pub(crate) fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(0))
}

/// `~/.overseer/agent/auth.json`, read once and rewritten on every change.
pub(crate) struct Store {
    path: PathBuf,
    credentials: Mutex<BTreeMap<String, Credential>>,
}

impl Store {
    /// A missing or unparsable file is an empty store: a corrupted file must cost one login, not
    /// a failed start, and the next `put` overwrites it.
    pub(crate) fn load(home: &Path) -> Self {
        let path = home.join(".overseer/agent/auth.json");
        let credentials =
            fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        Self { path, credentials: Mutex::new(credentials) }
    }

    fn guard(&self) -> MutexGuard<'_, BTreeMap<String, Credential>> {
        self.credentials.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn get(&self, url: &str) -> Option<Credential> {
        self.guard().get(url).cloned()
    }

    pub(crate) fn put(&self, credential: Credential) -> Result<(), String> {
        let url = credential.server_url.clone();
        let previous = self.guard().insert(url.clone(), credential);
        // Serialising and fsyncing outside the lock keeps a slow disk from stalling every reader.
        let text = serde_json::to_string_pretty(&*self.guard()).map_err(|e| format!("auth.json: {e}"))?;
        match write_private(&self.path, &text) {
            Ok(()) => Ok(()),
            Err(e) => {
                // A credential that was never persisted must not be handed out by `get`.
                let mut known = self.guard();
                match previous {
                    Some(previous) => known.insert(url, previous),
                    None => known.remove(&url),
                };
                Err(e)
            }
        }
    }
}

/// Writes `text` where only the user can read it, atomically: a crash must not truncate the store.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| crate::config::io_error(dir.display(), &e))?;
    }
    // Unique per process and created fresh: `mode(0o600)` only applies to a new inode, and a
    // planted symlink or leftover file must not redirect or widen the write.
    let temporary = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        process::id()
    ));
    let _ = fs::remove_file(&temporary);
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    };
    write().map_err(|e| crate::config::io_error(temporary.display(), &e))?;
    fs::rename(&temporary, path).map_err(|e| crate::config::io_error(path.display(), &e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn credential(url: &str) -> Credential {
        Credential {
            server_url: url.into(),
            authorization_url: "https://auth.example/authorize".into(),
            token_url: "https://auth.example/token".into(),
            client_id: "client".into(),
            client_secret: None,
            access_token: "token".into(),
            refresh_token: Some("refresh".into()),
            expires_at: now() + 3600,
            scopes: None,
            resource: None,
        }
    }

    #[test]
    fn a_credential_survives_a_round_trip_and_the_file_is_private() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::load(home.path());
        assert!(store.get("https://x/mcp").is_none());
        store.put(credential("https://x/mcp")).unwrap();
        let path = home.path().join(".overseer/agent/auth.json");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token file is readable by its owner alone");
        let reopened = Store::load(home.path());
        assert_eq!(reopened.get("https://x/mcp").map(|c| c.access_token), Some("token".to_string()));
    }

    #[test]
    fn a_planted_temporary_file_cannot_widen_the_store_permissions() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".overseer/agent");
        fs::create_dir_all(&dir).unwrap();
        let temporary = dir.join(format!("auth.json.tmp.{}", process::id()));
        fs::write(&temporary, "planted").unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644)).unwrap();
        Store::load(home.path()).put(credential("https://x/mcp")).unwrap();
        let mode = fs::metadata(dir.join("auth.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a leftover temp file must not carry its mode into the store");
    }

    #[test]
    fn a_credential_debug_redacts_its_secrets() {
        let mut secret = credential("https://x/mcp");
        secret.client_secret = Some("s3cret".into());
        let text = format!("{secret:?}");
        assert!(!text.contains("s3cret"), "{text}");
        assert!(!text.contains("\"token\""), "{text}");
        assert!(!text.contains("\"refresh\""), "{text}");
    }

    #[test]
    fn an_unparsable_store_is_empty_and_the_next_write_replaces_it() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".overseer/agent");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("auth.json"), "{not json").unwrap();
        let store = Store::load(home.path());
        assert!(store.get("https://x/mcp").is_none());
        store.put(credential("https://x/mcp")).unwrap();
        assert!(Store::load(home.path()).get("https://x/mcp").is_some());
    }

    #[test]
    fn a_credential_past_its_deadline_is_expired() {
        let mut soon = credential("https://x/mcp");
        soon.expires_at = now() - 1;
        assert!(soon.expired());
        assert!(!credential("https://x/mcp").expired());
    }
}
