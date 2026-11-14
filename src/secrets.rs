use anyhow::{Context, Result, bail};
use std::{env, path::Path};

/// Name of the environment variable holding the API key.
const API_KEY: &str = "OVERSEER_API_KEY";

/// `OVERSEER_API_KEY` from the environment, else from `$XDG_CONFIG_HOME/overseer/.env`.
pub(crate) fn api_key(home: &Path) -> Result<String> {
    if let Some(key) = env_var(API_KEY) {
        return Ok(key);
    }
    let path = crate::config::path(home).with_file_name(".env");
    let missing = || format!("{API_KEY} is not set; set it in the environment or in {}", path.display());
    let items = match dotenvy::from_path_iter(&path) {
        Ok(items) => items,
        Err(e) if e.not_found() => return Err(anyhow::Error::msg(missing())),
        Err(e) => return Err(anyhow::Error::new(e)).with_context(|| format!("reading {}", path.display())),
    };
    let mut blank = false;
    for item in items {
        let (key, value) = item.with_context(|| format!("reading {}", path.display()))?;
        if key == API_KEY {
            let value = value.trim();
            if value.is_empty() {
                blank = true;
            } else {
                return Ok(value.to_string());
            }
        }
    }
    if blank {
        bail!("{API_KEY} is empty in {}", path.display());
    }
    Err(anyhow::Error::msg(missing()))
}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}
