//! Agent profiles: `~/.overseer/agent/profiles/<name>.md` gives one agent a flavor, a model, an
//! effort and further restrictions. Every agent keeps the one implementation and the one tool set.

use crate::tools;
use gray_matter::{Matter, engine::YAML};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Where the user keeps one profile per file, under their home.
const ROOT: &str = ".overseer/agent/profiles";

/// The profile the first agent always has; a user file of this name overrides the built-in text.
pub(crate) const ORCHESTRATOR: &str = "orchestrator";

/// The reasoning efforts a profile may ask for.
pub(crate) const EFFORTS: &[&str] = &["low", "medium", "high"];

/// The built-in orchestrator, read through `profile://orchestrator` and used when the user has no
/// file of that name: the root's flavor, and the flavour of any agent the user gives it to.
const ORCHESTRATOR_FILE: &str = "---\ndescription: Splits the work, delegates it, and answers the user.\nkeywords: [orchestrate, delegate, split, plan, coordinate]\n---\nYou coordinate. Decide what is worth doing, split it into slices one sub-agent can own alone, spawn each with complete instructions and the right profile, then read the reports and answer the user. Do the work yourself only when delegating would cost more than doing it.";

/// The fields a profile's frontmatter sets.
#[derive(Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    keywords: Option<Vec<String>>,
    model: Option<String>,
    effort: Option<String>,
    read_only: Option<bool>,
    deny_tools: Option<Vec<String>>,
}

/// One profile as its file states it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Profile {
    pub(crate) name: String,
    /// One line, for whoever picks a profile for a sub-agent.
    pub(crate) description: String,
    pub(crate) keywords: Vec<String>,
    /// The flavor appended to the agent prompt; the base prompt is not replaceable.
    pub(crate) body: String,
    pub(crate) model: Option<String>,
    /// Unvalidated on load: a typo is reported by the spawn that uses the profile, where a model
    /// can read it and pick another, not by a startup that would kill the session.
    pub(crate) effort: Option<String>,
    /// What the profile adds to its spawner's rights; `Policy::inherit` keeps it tightening-only.
    pub(crate) policy: tools::Policy,
    /// The file as loaded. `profile://` returns this text, never a re-read, so what a reader sees is
    /// exactly what the session runs; editing a profile takes a restart, like `config.toml`.
    raw: String,
}

impl Profile {
    /// The profile's effort, or why the backend cannot be asked for it.
    pub(crate) fn effort(&self) -> Result<Option<&str>, String> {
        match self.effort.as_deref() {
            Some(effort) if !EFFORTS.contains(&effort) => {
                Err(format!("profile {}: effort {effort:?} is not one of {}", self.name, EFFORTS.join(", ")))
            }
            other => Ok(other),
        }
    }
}

/// Every profile discoverable from the user's home, the built-in orchestrator included.
#[derive(Debug)]
pub(crate) struct Catalogue {
    profiles: Vec<Profile>,
    by_name: HashMap<String, usize>,
}

impl Default for Catalogue {
    /// The built-in orchestrator alone: what a home with no profiles directory yields.
    fn default() -> Self {
        Self::build(Vec::new())
    }
}

impl Catalogue {
    /// Every `*.md` directly under the user root; a missing or unreadable root is no profile.
    pub(crate) fn load(home: &Path) -> Self {
        let mut found = Vec::new();
        if let Ok(entries) = fs::read_dir(home.join(ROOT)) {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
                .filter(|path| fs::metadata(path).is_ok_and(|meta| meta.is_file()))
                .collect();
            files.sort();
            found = files.iter().filter_map(|path| load(path)).collect();
        }
        Self::build(found)
    }

    /// Keeps one profile per name (files first, the built-in last) and orders them by name, so the
    /// catalogue and the error messages are identical from one run to the next.
    fn build(found: Vec<Profile>) -> Self {
        let mut seen = HashSet::new();
        let mut profiles: Vec<Profile> = found
            .into_iter()
            .chain(std::iter::once(builtin()))
            .filter(|profile| seen.insert(profile.name.clone()))
            .collect();
        profiles.sort_by(|a, b| a.name.cmp(&b.name));
        let by_name = profiles.iter().enumerate().map(|(index, profile)| (profile.name.clone(), index)).collect();
        Self { profiles, by_name }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Profile> {
        self.by_name.get(name).and_then(|index| self.profiles.get(*index))
    }

    /// The known names, for an error a model reads; the catalogue is never empty.
    pub(crate) fn names(&self) -> String {
        self.profiles.iter().map(|profile| profile.name.as_str()).collect::<Vec<&str>>().join(", ")
    }

    /// The catalogue as a prompt reads it: one line per profile, at least the orchestrator.
    pub(crate) fn catalogue(&self) -> String {
        self.profiles
            .iter()
            .map(|profile| {
                let mut line = format!("- {}: {}", profile.name, fold(&profile.description));
                if !profile.keywords.is_empty() {
                    let _ = write!(line, " (keywords: {})", profile.keywords.join(", "));
                }
                let mut rights: Vec<&str> = Vec::new();
                if profile.policy.read_only {
                    rights.push("read-only");
                }
                if !profile.policy.deny_tools.is_empty() {
                    rights.push("bans tools");
                }
                if !rights.is_empty() {
                    let _ = write!(line, " [{}]", rights.join(", "));
                }
                line
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    /// Reads a profile through `profile://<name>`: the file as loaded, frontmatter included.
    pub(crate) fn read(&self, target: &str) -> Result<String, String> {
        self.get(target)
            .map(|profile| profile.raw.clone())
            .ok_or_else(|| format!("unknown profile {target}; profiles are {}", self.names()))
    }
}

/// One profile file, or `None` when it cannot be read or its path has no file stem.
fn load(path: &Path) -> Option<Profile> {
    let text = fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let parsed = frontmatter(&text);
    Some(build_profile(parsed.fields, stem, parsed.body, text))
}

/// The built-in orchestrator profile.
fn builtin() -> Profile {
    let parsed = frontmatter(ORCHESTRATOR_FILE);
    build_profile(parsed.fields, ORCHESTRATOR.to_string(), parsed.body, ORCHESTRATOR_FILE.to_string())
}

/// A profile's frontmatter fields, its body, and nothing else.
struct Parsed {
    fields: Frontmatter,
    body: String,
}

/// Reads one file; malformed YAML yields no fields, so a bad profile still loads rather than
/// disappearing. The body is still taken after the closing fence, so a broken frontmatter block can
/// never leak its `---` lines and keys into the agent prompt.
fn frontmatter(text: &str) -> Parsed {
    match Matter::<YAML>::new().parse::<Frontmatter>(text) {
        Ok(parsed) => Parsed { fields: parsed.data.unwrap_or_default(), body: parsed.content.trim().to_string() },
        Err(_) => Parsed { fields: Frontmatter::default(), body: after_fence(text).to_string() },
    }
}

/// The text after the closing `---`, or the whole text when the file has no frontmatter at all. An
/// opened but unclosed fence has no body, so a broken block never leaks its keys into a prompt.
fn after_fence(text: &str) -> &str {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return text.trim();
    };
    let Some(rest) = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n")) else {
        return text.trim();
    };
    rest.split_once("\n---").map_or("", |(_, after)| after.trim())
}

/// The profile one file describes; `stem` is its file name without `.md`.
fn build_profile(fields: Frontmatter, stem: String, body: String, raw: String) -> Profile {
    // A declared name reaches prompts and `profile://`: an unsafe one falls back to the file stem.
    let safe = |name: &str| !name.contains('/') && !name.contains('\\') && !name.chars().any(char::is_control);
    let declared = fields.name.as_deref().map(str::trim).filter(|name| !name.is_empty()).filter(|name| safe(name));
    let name = declared.map_or(stem, str::to_string);
    let read_only = fields.read_only.unwrap_or(false);
    let deny_tools = fields.deny_tools.unwrap_or_default();
    Profile {
        name,
        description: fields.description.unwrap_or_default(),
        keywords: fields.keywords.unwrap_or_default(),
        body,
        model: fields.model.map(|model| model.trim().to_string()).filter(|model| !model.is_empty()),
        effort: fields.effort.map(|effort| effort.trim().to_string()).filter(|effort| !effort.is_empty()),
        policy: tools::Policy::default().set(Some(read_only), Some(deny_tools), None, None),
        raw,
    }
}

/// `text` with every run of whitespace collapsed to one space.
fn fold(text: &str) -> String {
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_orchestrator_exists_even_with_no_directory() {
        let home = tempfile::tempdir().unwrap();
        let catalogue = Catalogue::load(home.path());
        let orchestrator = catalogue.get(ORCHESTRATOR).expect("the root always has a profile");
        assert!(orchestrator.body.starts_with("You coordinate."), "{:?}", orchestrator.body);
        assert_eq!(orchestrator.policy, tools::Policy::default());
        assert!(orchestrator.model.is_none());
        assert!(catalogue.catalogue().starts_with(&format!("- {ORCHESTRATOR}: ")));
    }

    #[test]
    fn a_file_overrides_the_built_in_orchestrator() {
        let home = tempfile::tempdir().unwrap();
        profile(home.path(), "orchestrator", "description: mine", "my own coordination");
        let catalogue = Catalogue::load(home.path());
        let orchestrator = catalogue.get(ORCHESTRATOR).unwrap();
        assert_eq!(orchestrator.body, "my own coordination");
        assert_eq!(orchestrator.description, "mine");
    }

    #[test]
    fn a_profile_carries_its_flavour_model_and_tightening() {
        let home = tempfile::tempdir().unwrap();
        profile(
            home.path(),
            "reviewer",
            "description: Reviews a diff.\nkeywords: [review, audit]\nmodel: main/coding\neffort: high\nread_only: true\ndeny_tools: [spawn_agent]",
            "You review.",
        );
        let catalogue = Catalogue::load(home.path());
        let reviewer = catalogue.get("reviewer").unwrap();
        assert_eq!(reviewer.description, "Reviews a diff.");
        assert_eq!(reviewer.keywords, ["review", "audit"]);
        assert_eq!(reviewer.body, "You review.");
        assert_eq!(reviewer.model.as_deref(), Some("main/coding"));
        assert_eq!(reviewer.effort().unwrap(), Some("high"));
        assert!(reviewer.policy.read_only);
        assert_eq!(reviewer.policy.deny_tools, ["spawn_agent"]);
    }

    #[test]
    fn a_bad_effort_is_refused_by_name_and_never_by_startup() {
        let home = tempfile::tempdir().unwrap();
        profile(home.path(), "fast", "description: x\neffort: ultra", "body");
        let catalogue = Catalogue::load(home.path());
        let fast = catalogue.get("fast").unwrap();
        let error = fast.effort().unwrap_err();
        assert!(error.contains("ultra") && error.contains("low, medium, high"), "{error}");
    }

    #[test]
    fn the_first_file_wins_a_duplicate_name() {
        let home = tempfile::tempdir().unwrap();
        profile(home.path(), "a", "name: shared\ndescription: first", "one");
        profile(home.path(), "b", "name: shared\ndescription: second", "two");
        let catalogue = Catalogue::load(home.path());
        assert_eq!(catalogue.get("shared").unwrap().description, "first");
    }

    #[test]
    fn a_profile_reads_through_its_uri_and_unknown_ones_name_the_known() {
        let home = tempfile::tempdir().unwrap();
        profile(home.path(), "reviewer", "description: Reviews a diff.", "You review.");
        let catalogue = Catalogue::load(home.path());
        let read = catalogue.read("reviewer").unwrap();
        assert!(read.contains("You review.") && read.contains("description: Reviews a diff."), "{read}");
        assert!(catalogue.read(ORCHESTRATOR).unwrap().contains("You coordinate."));
        let error = catalogue.read("nope").unwrap_err();
        assert!(error.contains("nope") && error.contains("reviewer"), "{error}");
    }

    #[test]
    fn a_malformed_profile_still_loads_with_its_stem() {
        let home = tempfile::tempdir().unwrap();
        profile(home.path(), "broken", "description: a: b", "still here");
        let catalogue = Catalogue::load(home.path());
        let broken = catalogue.get("broken").expect("a malformed block never drops a profile");
        assert_eq!(broken.description, "");
        assert_eq!(broken.body, "still here");
    }

    /// Writes one profile file under the user root `home` names.
    fn profile(home: &Path, file: &str, frontmatter: &str, body: &str) {
        let dir = home.join(ROOT);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{file}.md")), format!("---\n{frontmatter}\n---\n{body}\n")).unwrap();
    }
}
