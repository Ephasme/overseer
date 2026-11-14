use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const EXAMPLE: &str = "roots = [\"~/code\"]\nbase_url = \"https://api.openai.com/v1\"\nmodel = \"gpt-4.1\"\n# Delegations allowed below the root agent; 0 forbids spawning.\nmax_depth = 3\n# Where the root agent starts.\ndir = \"~/code\"";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    roots: Vec<String>,
    base_url: String,
    model: String,
    /// Omitted means `auto`.
    theme: Option<String>,
    max_depth: Option<usize>,
    dir: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Config {
    /// Directories scanned for repositories by `list_repos` and listed in the overseer prompt; not a sandbox.
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) theme: Theme,
    /// Delegations allowed below the root; `0` means the root may not spawn anyone.
    pub(crate) max_depth: usize,
    /// The directory the root agent starts in; children get the path `spawn_agent` names.
    pub(crate) dir: PathBuf,
    /// Resolved once at startup, so no later call re-reads the environment.
    pub(crate) home: PathBuf,
}

/// Delegations allowed below the root when `config.toml` names no `max_depth`.
pub(crate) const DEFAULT_MAX_DEPTH: usize = 3;

/// Which palette the TUI draws with; `auto` is the one that reads `NO_COLOR`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Theme {
    #[default]
    Auto,
    Dark,
    Light,
    /// No colour at all: every slot is the terminal's default and selection is reverse video.
    Mono,
}

impl Theme {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            "mono" => Ok(Self::Mono),
            other => bail!("theme {other:?} is not auto, dark, light or mono"),
        }
    }
}

/// The user's home directory; an unset or empty `HOME` is an error, not a relative path.
pub(crate) fn home() -> Result<PathBuf> {
    env::var_os("HOME").filter(|value| !value.is_empty()).map(PathBuf::from).context("HOME is not set")
}

pub(crate) fn path(home: &Path) -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config", home).join("overseer/config.toml")
}

pub(crate) fn state_dir(home: &Path) -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state", home).join("overseer")
}

/// The `<path>: <os error>` shape every tool, log and path error uses.
pub(crate) fn io_error(path: impl std::fmt::Display, e: &std::io::Error) -> String {
    format!("{path}: {e}")
}

/// Environment variable that echoes model requests and tool results to stderr.
pub(crate) const DEBUG: &str = "OVERSEER_DEBUG";

/// Environment variable that forces line mode even on a terminal.
pub(crate) const PLAIN: &str = "OVERSEER_PLAIN";

/// `$XDG_*_HOME` is absolute by spec: a relative value would put config and logs under the CWD.
fn xdg(var: &str, fallback: &str, home: &Path) -> PathBuf {
    env::var_os(var).map(PathBuf::from).filter(|value| value.is_absolute()).unwrap_or_else(|| home.join(fallback))
}

pub(crate) fn load(home: &Path) -> Result<Config> {
    let path = path(home);
    let text = fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::Error::msg(format!("cannot read {}; create it with:\n{EXAMPLE}", path.display()))
        } else {
            anyhow::Error::new(e).context(format!("cannot read {}", path.display()))
        }
    })?;
    parse(&text, home).with_context(|| format!("invalid config {}", path.display()))
}

fn parse(text: &str, home: &Path) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text)?;
    if raw.roots.is_empty() {
        bail!("roots is empty");
    }
    if raw.base_url.trim().is_empty() || raw.model.trim().is_empty() {
        bail!("base_url and model must not be empty");
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
    let theme = raw.theme.as_deref().map_or(Ok(Theme::Auto), Theme::parse)?;
    // The root works in the user's home, not in the world-writable directory a child falls back to.
    let dir = canonical_dir(raw.dir.as_deref().unwrap_or("~"), home)?;
    Ok(Config {
        roots,
        base_url: raw.base_url,
        model: raw.model,
        theme,
        max_depth: raw.max_depth.unwrap_or(DEFAULT_MAX_DEPTH),
        dir,
        home: home.to_path_buf(),
    })
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

/// Canonical directory a worker works in, or why the path is unusable.
pub(crate) fn resolve_dir(path: &str, home: &Path) -> Result<PathBuf, String> {
    canonical_dir(path, home).map_err(|error| format!("{error}"))
}

/// As [`resolve_dir`], typed so `parse` keeps the OS error chain.
fn canonical_dir(path: &str, home: &Path) -> Result<PathBuf> {
    let expanded = expand_home(path, home);
    let canonical = fs::canonicalize(&expanded).map_err(|e| {
        let message = io_error(expanded.display(), &e);
        anyhow::Error::new(e).context(message)
    })?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
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
        assert_eq!(config.home, home.path());
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
        let blank = parse("roots = [\"/\"]\nbase_url = \" \"\nmodel = \"m\"", home.path()).unwrap_err().to_string();
        assert!(blank.contains("must not be empty"), "{blank}");
    }

    #[test]
    fn resolve_dir_accepts_existing_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        assert_eq!(
            resolve_dir(dir.path().to_str().unwrap(), dir.path()).unwrap(),
            fs::canonicalize(dir.path()).unwrap()
        );
        let canonical = fs::canonicalize(&file).unwrap();
        assert_eq!(
            resolve_dir(file.to_str().unwrap(), dir.path()).unwrap_err(),
            format!("{} is not a directory", canonical.display())
        );
        assert!(resolve_dir("/nonexistent/overseer", dir.path()).unwrap_err().starts_with("/nonexistent/overseer:"));
    }

    #[test]
    fn resolve_dir_expands_home_against_the_given_home() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("code")).unwrap();
        assert_eq!(resolve_dir("~/code", home.path()).unwrap(), fs::canonicalize(home.path().join("code")).unwrap());
    }

    #[test]
    fn theme_defaults_to_auto_and_rejects_what_it_does_not_know() {
        let home = tempfile::tempdir().unwrap();
        let rest = "roots = [\"/\"]\nbase_url = \"u\"\nmodel = \"m\"";
        assert_eq!(parse(rest, home.path()).unwrap().theme, Theme::Auto);
        assert_eq!(parse(&format!("{rest}\ntheme = \"light\""), home.path()).unwrap().theme, Theme::Light);
        let wrong = parse(&format!("{rest}\ntheme = \"solarized\""), home.path()).unwrap_err().to_string();
        assert!(wrong.contains("solarized"), "{wrong}");
    }

    #[test]
    fn depth_and_root_directory_default_when_absent() {
        let home = tempfile::tempdir().unwrap();
        let text = r#"roots = ["/tmp"]
base_url = "http://localhost:8080/v1"
model = "m""#;
        let resolved = parse(text, home.path()).unwrap();
        assert_eq!(resolved.max_depth, DEFAULT_MAX_DEPTH);
        assert_eq!(DEFAULT_MAX_DEPTH, 3);
        assert_eq!(resolved.dir, fs::canonicalize(home.path()).unwrap());
    }

    #[test]
    fn depth_and_root_directory_are_read_when_present() {
        let text = r#"roots = ["/tmp"]
base_url = "http://localhost:8080/v1"
model = "m"
max_depth = 1
dir = "/tmp""#;
        let resolved = parse(text, Path::new("/Users/nobody")).unwrap();
        assert_eq!(resolved.max_depth, 1);
    }

    #[test]
    fn a_root_directory_that_is_not_a_directory_is_refused() {
        let text = r#"roots = ["/tmp"]
base_url = "http://localhost:8080/v1"
model = "m"
dir = "/nonexistent/x""#;
        let error = parse(text, Path::new("/Users/nobody")).unwrap_err().to_string();
        assert!(error.contains("/nonexistent/x"), "{error}");
    }
}
