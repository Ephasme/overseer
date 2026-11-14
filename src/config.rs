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

/// Canonical repository directory, or why the worker may not use it.
pub(crate) fn resolve_repo(roots: &[PathBuf], repo: &str) -> Result<PathBuf, String> {
    let path = fs::canonicalize(expand_home(repo, &home())).map_err(|e| format!("{repo}: {e}"))?;
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    if roots.iter().any(|root| path.starts_with(root)) {
        return Ok(path);
    }
    let allowed: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    Err(format!("{} is outside the configured roots: {}", path.display(), allowed.join(", ")))
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
    fn resolve_repo_enforces_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("repo")).unwrap();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        let inside = root.path().join("repo");
        assert_eq!(resolve_repo(&roots, inside.to_str().unwrap()).unwrap(), fs::canonicalize(&inside).unwrap());
        assert!(
            resolve_repo(&roots, outside.path().to_str().unwrap())
                .unwrap_err()
                .contains("is outside the configured roots")
        );
        assert!(resolve_repo(&roots, "/nonexistent/overseer").unwrap_err().starts_with("/nonexistent/overseer:"));
    }
}
