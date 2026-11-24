use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// The CLI's configuration file: where the model is and which one to ask.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub send_reasoning: bool,
    /// Model calls one turn may make; the agent's own default when absent.
    #[serde(default)]
    pub max_steps: Option<usize>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    /// The model's API key, from the environment; absent is a valid configuration for a local
    /// endpoint that needs none.
    #[must_use]
    pub fn api_key() -> Option<String> {
        std::env::var("OVERSEER_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
    }

    /// `OVERSEER_CONFIG` if set, otherwise `~/.config/overseer/config.toml`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        std::env::var_os("OVERSEER_CONFIG").map_or_else(
            || {
                std::env::var_os("HOME").map_or_else(
                    || PathBuf::from("overseer.toml"),
                    |home| PathBuf::from(home).join(".config/overseer/config.toml"),
                )
            },
            PathBuf::from,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn parses_a_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "base_url = \"http://x/v1\"\nmodel = \"m\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.base_url, "http://x/v1");
        assert_eq!(config.model, "m");
        assert!(!config.send_reasoning);
        assert!(config.max_steps.is_none());
    }

    #[test]
    fn rejects_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "base_url = \"u\"\nmodel = \"m\"\nextra = 1\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
