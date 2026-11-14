//! OAuth credentials, one per server URL, in a file only the user can read.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// What one server's login produced, and what a refresh needs to renew it.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        let mut known = self.guard();
        known.insert(credential.server_url.clone(), credential);
        let text = serde_json::to_string_pretty(&*known).map_err(|e| format!("auth.json: {e}"))?;
        write_private(&self.path, &text)
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
    let temporary = path.with_file_name(format!("{}.tmp", path.file_name().unwrap_or_default().to_string_lossy()));
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
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
