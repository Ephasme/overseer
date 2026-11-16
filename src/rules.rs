//! The instruction files that govern a directory: the ones a coding agent would read on entry.

use std::fs;
use std::path::{Path, PathBuf};

/// The file names an agent reads as rules, relative to a directory.
const NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", "RULES.md", ".cursorrules", ".github/copilot-instructions.md"];

/// Every rule file that governs `dir`: from the repository root (where the walk from `dir` meets a
/// `.git`) down to `dir`, then the configuration's. Without a repository root only `dir` itself
/// counts: rule files from arbitrary ancestors must not feed the prompt automatically.
fn found(config_dir: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut walk: Vec<PathBuf> = Vec::new();
    let mut rooted = false;
    for ancestor in dir.ancestors() {
        walk.push(ancestor.to_path_buf());
        if ancestor.join(".git").exists() {
            rooted = true;
            break;
        }
    }
    let bases = if rooted {
        walk.iter().rev().map(PathBuf::as_path).chain(std::iter::once(config_dir)).collect::<Vec<_>>()
    } else {
        vec![dir, config_dir]
    };
    for base in bases {
        for name in NAMES {
            let path = base.join(name);
            let Ok(resolved) = path.canonicalize() else { continue };
            if resolved.is_file() && !seen.contains(&resolved) {
                seen.push(resolved);
            }
        }
    }
    seen
}

/// One discovered rule file: loaded with its text and truncation flag, or skipped on a read error.
type Entry = (PathBuf, Option<(String, bool)>);

/// The rule files an agent's prompt carries, loaded once at thread start and reused every turn.
pub(crate) struct Rules {
    /// Every discovered file in discovery order: repository root first, the agent's directory
    /// last, then the configuration's.
    entries: Vec<Entry>,
}

impl Rules {
    /// Reads every discovered rule file. A file that cannot be read is skipped, not fatal: the
    /// prompt names it and `rule://` still surfaces the error on demand.
    pub(crate) fn load(config_dir: &Path, dir: &Path) -> Self {
        let entries = found(config_dir, dir)
            .into_iter()
            .map(|path| match crate::tools::read_limited(&path, &path.display().to_string()) {
                Ok((text, cut)) => (path, Some((text, cut))),
                Err(_) => (path, None),
            })
            .collect();
        Self { entries }
    }

    /// Empty means no `<rules>` section at all: neither content nor a skip note.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates `(path, Some((text, truncated)))` per loaded file, `(path, None)` per skipped one,
    /// in discovery order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }
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

    #[test]
    fn rules_load_in_discovery_order_and_skip_unreadable_files() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let deep = repo.join("src/nested");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join("AGENTS.md"), "repo root").unwrap();
        fs::write(deep.join("AGENTS.md"), "deep dir").unwrap();
        let config_dir = root.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("RULES.md"), "config rules").unwrap();

        let rules = Rules::load(&config_dir, &deep);
        let order: Vec<String> =
            rules.iter().map(|(path, _)| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(order, ["AGENTS.md", "AGENTS.md", "RULES.md"], "repository root first, then deeper, then config");
        assert!(!rules.is_empty());
        let contents: Vec<&str> = rules.iter().filter_map(|(_, l)| l.as_ref().map(|(t, _)| t.as_str())).collect();
        assert_eq!(contents, ["repo root", "deep dir", "config rules"]);

        // A rule file the loader cannot read is skipped, not fatal. Non-UTF-8 bytes are one such
        // refusal that holds for any user, root included.
        let broken = repo.join("CLAUDE.md");
        fs::write(&broken, [0xff, 0xfe]).unwrap();
        let rules = Rules::load(&config_dir, &deep);
        let skipped: Vec<PathBuf> = rules.iter().filter(|(_, l)| l.is_none()).map(|(p, _)| p.clone()).collect();
        assert_eq!(skipped, [broken.canonicalize().unwrap()], "only the unreadable file is skipped");
    }

    #[test]
    fn no_rule_files_means_empty() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let rules = Rules::load(&config_dir, root.path());
        assert!(rules.is_empty());
        assert_eq!(rules.iter().count(), 0);
    }
}
