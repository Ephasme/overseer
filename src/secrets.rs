use anyhow::{Context, Result};
use std::{env, path::Path};

/// Name of the environment variable holding the API key.
const API_KEY: &str = "OVERSEER_API_KEY";

/// `OVERSEER_API_KEY` from the environment, else from `~/.config/overseer/.env`.
pub(crate) fn api_key(home: &Path) -> Result<String> {
    if let Some(key) = env_var(API_KEY) {
        return Ok(key);
    }
    let path = crate::config::path(home).with_file_name(".env");
    let context = || format!("{API_KEY} is not set; set it in the environment or in {}", path.display());
    if !path.exists() {
        return Err(anyhow::Error::msg(context()));
    }
    for item in dotenvy::from_path_iter(&path).with_context(|| format!("reading {}", path.display()))? {
        let (key, value) = item.with_context(|| format!("reading {}", path.display()))?;
        if key == API_KEY && !value.trim().is_empty() {
            return Ok(value.trim().to_string());
        }
    }
    Err(anyhow::Error::msg(context()))
}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}
