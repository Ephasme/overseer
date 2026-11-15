//! The instruction files that govern a directory: the ones a coding agent would read on entry.

use std::fs;
use std::path::{Path, PathBuf};

/// The file names an agent reads as rules, relative to a directory.
const NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", "RULES.md", ".cursorrules", ".github/copilot-instructions.md"];

/// Every rule file that governs `dir`: from it up to the repository root, then the configuration's.
fn found(config_dir: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut walk: Vec<PathBuf> = Vec::new();
    for ancestor in dir.ancestors() {
        walk.push(ancestor.to_path_buf());
        if ancestor.join(".git").exists() {
            break;
        }
    }
    walk.push(config_dir.to_path_buf());
    for base in walk {
        for name in NAMES {
            let path = base.join(name);
            let Ok(resolved) = path.canonicalize() else { continue };
            if resolved.is_file() && !seen.contains(&resolved) {
                seen.push(resolved);
            }
        }
    }
    seen.sort();
    seen
}

/// What `rule://` lists: every rule file that governs `dir`, with its size.
pub(crate) fn list(config_dir: &Path, dir: &Path) -> String {
    let files = found(config_dir, dir);
    if files.is_empty() {
        return format!("no rule files found from {}", dir.display());
    }
    files
        .iter()
        .map(|path| match fs::metadata(path) {
            Ok(meta) => format!("rule://{} ({} bytes)", path.display(), meta.len()),
            Err(_) => format!("rule://{} (unreadable)", path.display()),
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// One rule file, named absolutely or relative to `dir`, provided it is one of the discovered set.
pub(crate) fn read(config_dir: &Path, dir: &Path, target: &str) -> Result<(String, bool), String> {
    let named = Path::new(target);
    let candidate = if named.is_absolute() { named.to_path_buf() } else { dir.join(named) };
    let resolved = candidate.canonicalize().map_err(|e| crate::config::io_error(target, &e))?;
    if !found(config_dir, dir).contains(&resolved) {
        return Err(format!("{} is not a rule file", resolved.display()));
    }
    crate::tools::read_limited(&resolved, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_around_a_repository_are_listed_and_only_they_are_readable() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let deep = repo.join("src/nested");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join("AGENTS.md"), "be brief").unwrap();
        fs::write(root.path().join("AGENTS.md"), "outside the repository").unwrap();
        fs::write(deep.join("notes.txt"), "not a rule").unwrap();
        let config_dir = root.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("RULES.md"), "global rules").unwrap();

        let listing = list(&config_dir, &deep);
        assert!(listing.contains("AGENTS.md"), "{listing}");
        assert!(listing.contains("RULES.md"), "{listing}");
        assert!(!listing.contains("outside the repository"), "the walk stops at the repository root: {listing}");

        let agents = repo.join("AGENTS.md");
        let (text, cut) = read(&config_dir, &deep, &agents.display().to_string()).unwrap();
        assert_eq!(text, "be brief");
        assert!(!cut);
        assert_eq!(read(&config_dir, &deep, "../../AGENTS.md").unwrap().0, "be brief", "a relative path works too");
        assert!(read(&config_dir, &deep, "notes.txt").unwrap_err().ends_with("notes.txt is not a rule file"));
        assert_eq!(
            read(&config_dir, &deep, "absent.md").unwrap_err(),
            "absent.md: No such file or directory (os error 2)"
        );
        let refused = read(&config_dir, &deep, &deep.join("notes.txt").display().to_string()).unwrap_err();
        assert!(refused.ends_with("is not a rule file"), "{refused}");
    }
}
