//! Skills as Oh my Pi lays them out: one level of `<root>/<name>/SKILL.md`, read with `skill://`.

use crate::config;
use crate::tools;
use gray_matter::{Matter, engine::YAML};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Roots under the user's home, first one winning a name collision.
const USER_ROOTS: &[&str] = &[".overseer/agent/skills", ".agents/skills", ".agent/skills", ".omp/agent/skills"];

/// Roots searched from a working directory up to the project root.
const PROJECT_ROOTS: &[&str] = &[".agents/skills", ".agent/skills", ".overseer/skills"];

pub(crate) struct Skill {
    name: String,
    description: String,
    /// Loadable through `skill://` but left out of the catalogue the model reads.
    hidden: bool,
    dir: PathBuf,
}

/// Every skill discoverable from one starting point, in discovery order.
pub(crate) struct Catalogue {
    skills: Vec<Skill>,
    by_name: HashMap<String, usize>,
}

impl Catalogue {
    /// The skills every agent in every directory sees: the user's own.
    #[cfg(test)] // the tree always starts from a directory, so `in_dir` covers the product path.
    pub(crate) fn user(home: &Path) -> Self {
        let mut found = Vec::new();
        for suffix in USER_ROOTS {
            scan(&home.join(suffix), &mut found);
        }
        Self::build(found)
    }

    /// The user's skills plus those of the project `dir` sits in; the project wins a bare name.
    pub(crate) fn in_dir(home: &Path, dir: &Path) -> Self {
        let mut found = Vec::new();
        let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        for ancestor in dir.ancestors() {
            // Home is covered by the user roots, which come after the project's, so the walk stops
            // there instead of scanning every directory up to the filesystem root.
            if ancestor == home.as_path() || ancestor.canonicalize().is_ok_and(|path| path == home) {
                break;
            }
            for suffix in PROJECT_ROOTS {
                scan(&ancestor.join(suffix), &mut found);
            }
            if ancestor.join(".git").exists() {
                break;
            }
        }
        for suffix in USER_ROOTS {
            scan(&home.join(suffix), &mut found);
        }
        Self::build(found)
    }

    /// Indexes the found skills, keeping the first of two with the same name.
    fn build(found: Vec<Skill>) -> Self {
        let mut skills: Vec<Skill> = Vec::with_capacity(found.len());
        let mut by_name = HashMap::new();
        for skill in found {
            if by_name.contains_key(&skill.name) {
                continue;
            }
            by_name.insert(skill.name.clone(), skills.len());
            skills.push(skill);
        }
        Self { skills, by_name }
    }

    /// The catalogue as a prompt reads it: one line per visible skill; empty when there is none.
    pub(crate) fn catalogue(&self) -> String {
        self.skills
            .iter()
            .filter(|skill| !skill.hidden)
            .map(|skill| {
                let description = fold(&skill.description);
                if description.is_empty() {
                    format!("- {}", skill.name)
                } else {
                    format!("- {}: {description}", skill.name)
                }
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// Reads a skill through `skill://`: `target` is the URI without its scheme.
    pub(crate) fn read(&self, target: &str) -> Result<String, String> {
        let (name, rest) = target.split_once('/').unwrap_or((target, ""));
        let Some(index) = self.by_name.get(name) else {
            return Err(format!("unknown skill {name}; skills are discovered when the agent starts"));
        };
        let Some(skill) = self.skills.get(*index) else {
            return Err(format!("unknown skill {name}"));
        };
        let label = if rest.is_empty() { "SKILL.md" } else { rest };
        if rest.starts_with('/') || Path::new(rest).components().any(|part| part == Component::ParentDir) {
            return Err(format!("{name}: {label} is outside the skill directory"));
        }
        let full = if rest.is_empty() { skill.dir.join("SKILL.md") } else { skill.dir.join(rest) };
        let resolved = match full.canonicalize() {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("{name}: {label}: no such file"));
            }
            Err(e) => return Err(config::io_error(label, &e)),
        };
        match skill.dir.canonicalize() {
            Ok(root) if !resolved.starts_with(&root) => {
                return Err(format!("{name}: {label} is outside the skill directory"));
            }
            Ok(_) => {}
            Err(e) => return Err(config::io_error(name, &e)),
        }
        if !resolved.is_file() {
            return Err(format!("{name}: {label} is not a regular file"));
        }
        let href = format!("skill://{target}");
        let (text, cut) = tools::read_limited(&resolved, &href)?;
        Ok(tools::with_note(text, cut))
    }
}

/// Adds every skill directly under `root`; a missing or unreadable root is not an error.
fn scan(root: &Path, out: &mut Vec<Skill>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter(|entry| fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir()))
        .map(|entry| entry.path())
        .collect();
    // A directory listing has no order of its own; a stable one keeps the catalogue reproducible.
    dirs.sort();
    for dir in dirs {
        if let Some(skill) = load(&dir) {
            out.push(skill);
        }
    }
}

/// The fields a `SKILL.md` frontmatter sets.
#[derive(Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
    hide: Option<bool>,
}

/// Reads the frontmatter of one file; a block that is malformed or absent yields no fields, so a
/// bad `SKILL.md` still loads instead of dropping out of the catalogue.
fn fields(text: &str) -> Frontmatter {
    Matter::<YAML>::new().parse::<Frontmatter>(text).ok().and_then(|parsed| parsed.data).unwrap_or_default()
}

/// One skill, or `None` when the directory carries no usable `SKILL.md`.
fn load(dir: &Path) -> Option<Skill> {
    let text = fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let fields = fields(&text);
    if fields.enabled == Some(false) {
        return None;
    }
    let declared = fields.name.as_deref().map(str::trim).filter(|name| !name.is_empty());
    let name = match declared {
        Some(name) => name.to_string(),
        None => dir.file_name()?.to_string_lossy().into_owned(),
    };
    if name.contains('/') || name.contains('\\') || name.chars().any(char::is_control) {
        return None;
    }
    Some(Skill {
        name,
        description: fields.description.unwrap_or_default(),
        hidden: fields.hide == Some(true),
        dir: dir.to_path_buf(),
    })
}

/// `text` with every run of whitespace collapsed to one space.
fn fold(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(root: &Path, name: &str, frontmatter: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), format!("---\n{frontmatter}\n---\nbody of {name}\n")).unwrap();
    }

    #[test]
    fn frontmatter_descriptions_fold_or_keep_their_lines() {
        let folded = fields("---\nname: x\ndescription: >-\n  one\n  two\n\n  three\n---\nbody\n");
        assert_eq!(folded.description.as_deref(), Some("one two\nthree"));
        let literal = fields("---\ndescription: |-\n  one\n  two\n---\n");
        assert_eq!(literal.description.as_deref(), Some("one\ntwo"));
        let kept = fields("---\ndescription: |\n  one\n---\n");
        assert_eq!(kept.description.as_deref(), Some("one\n"));
        let quoted = fields("---\ndescription: \"quoted: value\"\n---\n");
        assert_eq!(quoted.description.as_deref(), Some("quoted: value"));
        let commented = fields("---\ndescription: foo # comment\n---\n");
        assert_eq!(commented.description.as_deref(), Some("foo"));
        let absent = fields("no frontmatter here");
        assert!(absent.description.is_none() && absent.name.is_none());
        let broken = fields("---\ndescription: a: b\n---\nbody\n");
        assert!(broken.description.is_none(), "malformed YAML loads as no fields, never as a failure");
    }

    #[test]
    fn the_name_defaults_to_the_directory_and_a_slash_is_refused() {
        let root = tempfile::tempdir().unwrap();
        skill(root.path(), "caveman", "description: be brief");
        skill(root.path(), "wrong", "name: a/b\ndescription: escaped");
        skill(root.path(), "injected", "name: |-\n  a\n  b\ndescription: escaped");
        let catalogue = Catalogue::user(Path::new("/nonexistent-home"));
        let named = Catalogue::build(
            [root.path().join("caveman"), root.path().join("wrong"), root.path().join("injected")]
                .into_iter()
                .filter_map(|dir| load(&dir))
                .collect(),
        );
        assert_eq!(catalogue.catalogue(), "");
        assert_eq!(named.catalogue(), "- caveman: be brief");
    }

    #[test]
    fn enabled_false_skips_a_skill_and_hide_keeps_it_out_of_the_catalogue() {
        let root = tempfile::tempdir().unwrap();
        skill(root.path(), "off", "enabled: false\ndescription: never");
        skill(root.path(), "quiet", "hide: true\ndescription: loadable");
        skill(root.path(), "loud", "description: listed");
        let mut found = Vec::new();
        scan(root.path(), &mut found);
        let catalogue = Catalogue::build(found);
        assert_eq!(catalogue.catalogue(), "- loud: listed");
        assert!(catalogue.read("quiet").is_ok(), "a hidden skill stays loadable");
    }

    #[test]
    fn a_read_cannot_leave_the_skill_directory() {
        let root = tempfile::tempdir().unwrap();
        skill(root.path(), "caveman", "description: be brief");
        fs::write(root.path().join("secret.txt"), "outside").unwrap();
        fs::write(root.path().join("caveman").join("notes.md"), "inside").unwrap();
        let mut found = Vec::new();
        scan(root.path(), &mut found);
        let catalogue = Catalogue::build(found);
        assert!(catalogue.read("caveman").unwrap().starts_with("---"));
        assert_eq!(catalogue.read("caveman/notes.md").unwrap(), "inside");
        assert!(catalogue.read("caveman/../secret.txt").is_err());
        assert!(catalogue.read("caveman/missing.md").is_err());
        assert!(catalogue.read("nope").is_err());
    }

    #[test]
    fn a_nested_skill_is_not_discovered_and_the_first_root_wins() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        skill(first.path(), "shared", "description: first");
        skill(second.path(), "shared", "description: second");
        skill(&first.path().join("group"), "nested", "description: too deep");
        let mut found = Vec::new();
        scan(first.path(), &mut found);
        scan(second.path(), &mut found);
        let catalogue = Catalogue::build(found);
        assert_eq!(catalogue.catalogue(), "- shared: first");
    }

    #[test]
    fn a_project_skill_is_scanned_before_the_user_roots() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join(".git")).unwrap();
        skill(&home.path().join(".agents/skills"), "shared", "description: user");
        skill(&home.path().join(".agents/skills"), "only-user", "description: user only");
        skill(&project.path().join(".agents/skills"), "shared", "description: project");
        let catalogue = Catalogue::in_dir(home.path(), project.path());
        assert_eq!(
            catalogue.catalogue(),
            "- shared: project\n- only-user: user only",
            "the project skill keeps the bare name"
        );
    }
}
