use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const EXAMPLE: &str = "roots = [\"~/code\"]\nbase_url = \"https://api.openai.com/v1\"\nmodel = \"gpt-4.1\"";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    roots: Vec<String>,
    base_url: String,
    model: String,
}

#[derive(Debug)]
pub(crate) struct Config {
    /// Canonical directories under which workers may operate.
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) base_url: String,
    pub(crate) model: String,
}

pub(crate) fn path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("overseer/config.toml")
}

pub(crate) fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("overseer")
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var).filter(|v| !v.is_empty()).map_or_else(|| home().join(fallback), PathBuf::from)
}

pub(crate) fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read {}; create it with:\n{EXAMPLE}", path.display()))?;
    parse(&text, &home()).with_context(|| format!("invalid config {}", path.display()))
}

/// `OVERSEER_API_KEY` from the environment, else from `~/.config/overseer/.env`.
pub(crate) fn api_key() -> Result<String> {
    if let Some(key) = env_var("OVERSEER_API_KEY") {
        return Ok(key);
    }
    let path = path().with_file_name(".env");
    let context = || format!("OVERSEER_API_KEY is not set; set it in the environment or in {}", path.display());
    if !path.exists() {
        return Err(anyhow::Error::msg(context()));
    }
    for item in dotenvy::from_path_iter(&path).with_context(|| format!("reading {}", path.display()))? {
        let (key, value) = item.with_context(|| format!("reading {}", path.display()))?;
        if key == "OVERSEER_API_KEY" && !value.trim().is_empty() {
            return Ok(value.trim().to_string());
        }
    }
    Err(anyhow::Error::msg(context()))
}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

fn parse(text: &str, home: &Path) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;
    if raw.roots.is_empty() {
        bail!("roots is empty");
    }
    let mut roots = Vec::new();
    for root in &raw.roots {
        let path = expand_home(root, home);
        let canonical = fs::canonicalize(&path).with_context(|| format!("root {}", path.display()))?;
        if !canonical.is_dir() {
            bail!("root {} is not a directory", canonical.display());
        }
        roots.push(canonical);
    }
    Ok(Config { roots, base_url: raw.base_url, model: raw.model })
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

/// Canonical directory a worker works in, or why the path is unusable.
pub(crate) fn resolve_dir(path: &str) -> Result<PathBuf, String> {
    let expanded = expand_home(path, &home());
    let canonical = fs::canonicalize(&expanded).map_err(|e| format!("{}: {e}", expanded.display()))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_expands_home() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("code")).unwrap();
        let config = parse(EXAMPLE, home.path()).unwrap();
        assert_eq!(config.roots, [fs::canonicalize(home.path().join("code")).unwrap()]);
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4.1");
    }

    #[test]
    fn parse_rejects_empty_missing_or_unknown() {
        let home = tempfile::tempdir().unwrap();
        let rest = "base_url = \"u\"\nmodel = \"m\"";
        assert_eq!(parse(&format!("roots = []\n{rest}"), home.path()).unwrap_err().to_string(), "roots is empty");
        assert!(
            parse(&format!("roots = [\"/nonexistent/overseer\"]\n{rest}"), home.path())
                .unwrap_err()
                .to_string()
                .starts_with("root /nonexistent/overseer")
        );
        assert!(parse(&format!("roots = [\"/\"]\n{rest}\ntypo = 1"), home.path()).is_err());
        assert!(parse("roots = [\"/\"]\nmodel = \"m\"", home.path()).unwrap_err().to_string().contains("base_url"));
    }

    #[test]
    fn resolve_dir_accepts_existing_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        assert_eq!(resolve_dir(dir.path().to_str().unwrap()).unwrap(), fs::canonicalize(dir.path()).unwrap());
        let canonical = fs::canonicalize(&file).unwrap();
        assert_eq!(
            resolve_dir(file.to_str().unwrap()).unwrap_err(),
            format!("{} is not a directory", canonical.display())
        );
        assert!(resolve_dir("/nonexistent/overseer").unwrap_err().starts_with("/nonexistent/overseer:"));
    }
}
