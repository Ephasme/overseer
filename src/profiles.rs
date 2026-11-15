//! Agent profiles: `~/.overseer/agent/profiles/<name>.md` gives one agent a flavor, a model, an
//! effort, a role and the exact tools it may call. Every agent keeps the one implementation.

use crate::mcp;
use crate::tools;
use gray_matter::{Matter, engine::YAML};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Where the user keeps one profile per file, under their home.
const ROOT: &str = ".overseer/agent/profiles";

/// The profile the first agent always has; a user file of this name replaces the built-in text.
pub(crate) const SUPERVISOR: &str = "supervisor";
pub(crate) const ORCHESTRATOR: &str = "orchestrator";

/// The reasoning efforts a profile may ask for.
pub(crate) const EFFORTS: &[&str] = &["low", "medium", "high"];

/// What an agent is for. A supervisor runs the tree and never writes; a worker does the work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    #[default]
    Worker,
    Supervisor,
}

impl Role {
    /// The policy the agent actually runs under. A supervisor loses `write_file` and every MCP
    /// grant, and runs bash read-only, whatever its profile lists. Never stored in `Control`.
    pub(crate) fn restrict(self, policy: &tools::Policy) -> tools::Policy {
        match self {
            Self::Worker => policy.clone(),
            Self::Supervisor => {
                let mut policy = policy.clone();
                policy.allow_tools.retain(|tool| tool != tools::WRITE_FILE && !tool.starts_with(mcp::PREFIX));
                policy.read_only = true;
                policy
            }
        }
    }

    /// The rights paragraph of the prompt.
    pub(crate) fn describe(self, policy: &tools::Policy) -> String {
        match self {
            Self::Worker => policy.describe(),
            Self::Supervisor => {
                let mut out = policy.describe();
                out.push_str(
                    "Your role removes write_file and every MCP tool whatever your profile lists, and runs bash read-only. This restriction is yours alone: the agents you start get exactly the rights of the profile you name, which may be more than yours.\n",
                );
                out
            }
        }
    }
}

/// The profiles this build ships. A file of the same name under the user root replaces one whole.
const BUILTINS: &[(&str, &str)] = &[
    (SUPERVISOR, SUPERVISOR_FILE),
    (ORCHESTRATOR, ORCHESTRATOR_FILE),
    ("planner", PLANNER_FILE),
    ("plan-reviewer", PLAN_REVIEWER_FILE),
    ("worker", WORKER_FILE),
    ("build-reviewer", BUILD_REVIEWER_FILE),
    ("researcher", RESEARCHER_FILE),
    ("integrator", INTEGRATOR_FILE),
];

const SUPERVISOR_FILE: &str = "---\ndescription: Runs the tree; delegates every task and integrates nothing itself.\nkeywords: [supervise, delegate, monitor, challenge, review]\neffort: high\nrole: supervisor\nallow_tools: [read_file, bash, spawn_agent, message_agent, control_agent, list_agents, list_repos]\n---\nYou answer for the whole tree. Brief an orchestrator per task, demand a written plan, demand an independent review of that plan, demand a review of every build, and accept nothing as done without the commands that prove it. Read files yourself when a report must be checked; never edit one.";

const ORCHESTRATOR_FILE: &str = "---\ndescription: \"Owns one task end to end: plan, delegate the slices, integrate, report.\"\nkeywords: [orchestrate, split, worktree, integrate, merge]\neffort: high\nallow_tools: [bash, read_file, write_file, spawn_agent, message_agent, control_agent, list_agents, list_repos]\n---\nYou own one task from the brief to the merged result. Split it into slices one agent can own alone, each with complete instructions because it sees nothing of your context except what you write. Parallel work in one repository means one worktree per agent on its own branch: `git worktree add ../<repo>-<slug> -b <type>/<slug> <base-branch>`. Two agents editing the same checkout is a dispatch bug, not a merge problem to fix later. Base every worktree on the branch whose build is actually deployed or installed, not on whatever HEAD happens to be, and state that base branch in each agent's instructions. Orchestration is not done when agents report: integrate their branches one at a time, in dependency order, with a PR where the repository has a remote and a review flow and with a rebase (or rebase-then-fast-forward) where it is local only, so history stays linear and never merge-commit soup. Agents never merge or rebase each other's branches: an agent pushes its branch and opens a PR against the agreed base branch when it finishes, or leaves a local branch and says so when the repository has no remote. You own integration: merge in dependency order, ask the next agent to rebase onto the updated base between merges when the branches touch the same code, and verify build and tests green on the integrated result before you report. Your final report names the plan path, every branch, every merge, the commands that proved the result, and what remains untested. Call `close` with your report as your last tool call when you have nothing further to do.";

const PLANNER_FILE: &str = "---\ndescription: Turns a brief into a written plan another agent can execute without guessing.\nkeywords: [plan, design, decompose, spec]\neffort: high\nread_only: true\nallow_tools: [bash, read_file, list_repos]\n---\nYou write the plan, not the code. Read the code the plan touches before you describe it: every path, symbol and signature you name must exist, and anything you could not confirm is marked `unverified — confirm first`. Return the plan as your final answer, in Markdown, with: the goal in one sentence; the ordered tasks, each with the files it touches, the exact change, and how it is tested; the interfaces each task consumes and produces; the commands that prove the whole thing works; and the assumptions a reader could overturn. No placeholders, no \"handle edge cases\", no task that says \"similar to task N\". A plan an implementer must interpret is a failed plan.";

const PLAN_REVIEWER_FILE: &str = "---\ndescription: Attacks a plan before anyone builds it.\nkeywords: [review, plan, critique, risk]\neffort: high\nread_only: true\nallow_tools: [bash, read_file]\n---\nYou are the plan's adversary. Read the plan, then read the code it claims things about, and report only what would derail the build: a step that cannot be executed as written, a symbol or path that does not exist, a missing test or acceptance check, an ordering that cannot work, a requirement of the brief the plan silently drops, a decision left to the implementer. For each, name the plan line and the evidence. End with a verdict: `ready`, or `blocked` and the shortest list of changes that would make it ready. Do not rewrite the plan.";

const WORKER_FILE: &str = "---\ndescription: \"Carries out one task: code, machine configuration, a command, a migration.\"\nkeywords: [build, implement, code, test, configure, run]\nallow_tools: [bash, read_file, write_file]\n---\nYou carry out exactly the task you were briefed with, in the directory and on the branch it names. It may be code, configuration of this machine, a command to run, or a file to produce. Read the plan and the surrounding code before editing; follow the repository's existing patterns rather than inventing a second convention. When the task is code, write the test that proves the behaviour, watch it fail, make it pass, then run the narrowest check that exercises the change and the repository's own suite when it is cheap. Commit in small steps with messages in English. Push your branch and open a PR against the base branch your instruction names, or leave the branch local and say so when there is no remote. Never merge, never rebase another branch, never touch a checkout that is not yours. Your final report names the branch or the files, every command you ran with its result, and what you did not test. Call `close` with your report as your last tool call when you are done.";

const BUILD_REVIEWER_FILE: &str = "---\ndescription: Reviews a finished slice and runs the proof itself.\nkeywords: [review, diff, test, verify]\neffort: high\nread_only: true\nallow_tools: [bash, read_file]\n---\nYou review a slice you did not write. Read the diff against the base branch, then run the build and the tests yourself: a review that repeats the worker's claims without running anything is not a review. Report only what matters: a behaviour that is wrong or missing against the plan, a test that cannot fail, a regression the diff introduces, a convention the slice breaks, a claim in the report the code does not support. Name file and line for each. End with `approved`, or `changes requested` and the shortest list that would earn approval.";

const RESEARCHER_FILE: &str = "---\ndescription: Answers one question from primary sources, with citations and stated uncertainty.\nkeywords: [research, investigate, compare, evidence]\neffort: high\nread_only: true\nallow_tools: [bash, read_file, list_repos]\n---\nYou answer one question and nothing else. Go to primary sources: the code in this repository, its tests, its history, the upstream documentation, the specification. Cite every claim with a path and line, a URL, or a command and its output. Separate what you verified from what you infer, and say plainly what you could not determine. If the question has two defensible answers, give both, the evidence for each, and which you would pick and why. Never pad: a short answer with citations beats a survey.";

const INTEGRATOR_FILE: &str = "---\ndescription: Merges finished branches in dependency order and leaves the base green.\nkeywords: [merge, rebase, integrate, release]\nallow_tools: [bash, read_file, write_file]\n---\nYou integrate branches, one at a time, in the dependency order your instruction gives. Before each merge, rebase the branch onto the current base; when the branches touch the same code, ask the next worker to rebase rather than resolving its conflicts yourself. Keep history linear: rebase and fast-forward, never a merge commit unless the repository's own convention demands one. After each merge, run the build and the tests the plan names, and stop at the first red result instead of stacking the next merge on top of it. Your final report lists every branch, its merge order, the commands run after each merge with their result, and anything left unmerged and why.";

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
    /// What this profile grants, and the command rules that go with it.
    pub(crate) policy: tools::Policy,
    /// Whether the agent runs the tree or does the work; a supervisor is exempt from its owner's
    /// tool list.
    pub(crate) role: Role,
    /// Whether the file declares `allow_tools` at all: an absent list is refused, an empty one
    /// grants nothing, and the two must stay distinguishable.
    pub(crate) tools_declared: bool,
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

    /// This profile's tools, or why it may not run. `grantable` is `prompt::GRANTABLE_TOOLS`; passing
    /// it in keeps this module from depending on the prompt.
    pub(crate) fn tools(&self, grantable: &[&str]) -> Result<&[String], String> {
        if !self.tools_declared {
            return Err(format!(
                "profile {} declares no allow_tools; write `allow_tools: []` to grant nothing",
                self.name
            ));
        }
        match self
            .policy
            .allow_tools
            .iter()
            .find(|tool| !grantable.contains(&tool.as_str()) && !tool.starts_with(mcp::PREFIX))
        {
            Some(unknown) => Err(format!(
                "profile {}: unknown tool {unknown:?}; grantable tools are {} or mcp__<server>",
                self.name,
                grantable.join(", ")
            )),
            None => Ok(&self.policy.allow_tools),
        }
    }
}

/// Every profile discoverable from the user's home, the built-ins included.
#[derive(Debug)]
pub(crate) struct Catalogue {
    profiles: Vec<Profile>,
    by_name: HashMap<String, usize>,
}

impl Default for Catalogue {
    /// The built-in profiles alone: what a home with no profiles directory yields.
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

    /// Keeps one profile per name (files first, the built-ins last) and orders them by name, so the
    /// catalogue and the error messages are identical from one run to the next.
    fn build(found: Vec<Profile>) -> Self {
        let mut seen = HashSet::new();
        let mut profiles: Vec<Profile> =
            found.into_iter().chain(builtins()).filter(|profile| seen.insert(profile.name.clone())).collect();
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

    /// The catalogue as a prompt reads it: one line per profile, with the rights it grants, so a
    /// supervisor picks one from the line alone.
    pub(crate) fn catalogue(&self) -> String {
        self.profiles
            .iter()
            .map(|profile| {
                let mut line = format!("- {}: {}", profile.name, fold(&profile.description));
                if !profile.keywords.is_empty() {
                    let _ = write!(line, " (keywords: {})", profile.keywords.join(", "));
                }
                let mut rights: Vec<String> = Vec::new();
                if profile.policy.allow_tools.is_empty() {
                    rights.push("no tools".to_string());
                } else {
                    rights.push(format!("tools: {}", profile.policy.allow_tools.join(" ")));
                }
                if profile.policy.read_only {
                    rights.push("read-only".to_string());
                }
                let _ = write!(line, " [{}]", rights.join("; "));
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

/// The profiles this build ships, one per entry of `BUILTINS`.
fn builtins() -> Vec<Profile> {
    BUILTINS
        .iter()
        .map(|(name, raw)| {
            let parsed = frontmatter(raw);
            build_profile(parsed.fields, (*name).to_string(), parsed.body, (*raw).to_string())
        })
        .collect()
}
/// The fields a profile's frontmatter sets.
#[derive(Deserialize, Default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    keywords: Option<Vec<String>>,
    model: Option<String>,
    effort: Option<String>,
    role: Option<Role>,
    allow_tools: Option<Vec<String>>,
    read_only: Option<bool>,
    allow_commands: Option<Vec<String>>,
    deny_commands: Option<Vec<String>>,
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
    Profile {
        name,
        description: fields.description.unwrap_or_default(),
        keywords: fields.keywords.unwrap_or_default(),
        body,
        model: fields.model.map(|model| model.trim().to_string()).filter(|model| !model.is_empty()),
        effort: fields.effort.map(|effort| effort.trim().to_string()).filter(|effort| !effort.is_empty()),
        tools_declared: fields.allow_tools.is_some(),
        policy: tools::Policy::default().set(
            Some(read_only),
            fields.allow_tools,
            fields.allow_commands,
            fields.deny_commands,
        ),
        role: fields.role.unwrap_or_default(),
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
    fn the_built_in_profiles_cover_the_delegation_chain() {
        let home = tempfile::tempdir().unwrap();
        let catalogue = Catalogue::load(home.path());
        assert_eq!(catalogue.get(SUPERVISOR).unwrap().role, Role::Supervisor);
        for name in ["orchestrator", "planner", "plan-reviewer", "worker", "build-reviewer", "researcher", "integrator"]
        {
            let profile = catalogue.get(name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(profile.role, Role::Worker, "{name}");
            assert!(!profile.description.is_empty(), "{name}");
            assert!(profile.tools(crate::prompt::GRANTABLE_TOOLS).is_ok(), "{name}");
        }
        assert!(catalogue.get(ORCHESTRATOR).unwrap().body.contains("git worktree add"));
        assert!(catalogue.get("worker").unwrap().policy.allow_tools.iter().any(|tool| tool == tools::BASH));
        // Sorted by name, so the catalogue's first line is the profile a supervisor reads first.
        assert!(catalogue.catalogue().starts_with("- build-reviewer: "), "{}", catalogue.catalogue());
    }

    #[test]
    fn a_profile_without_allow_tools_is_refused_and_an_empty_one_grants_nothing() {
        let home = tempfile::tempdir().unwrap();
        profile(home.path(), "silent", "description: says nothing about tools", "body");
        profile(home.path(), "mute", "description: Thinks only.\nallow_tools: []", "You think.");
        let catalogue = Catalogue::load(home.path());
        let refused = catalogue.get("silent").unwrap().tools(crate::prompt::GRANTABLE_TOOLS).unwrap_err();
        assert!(refused.contains("declares no allow_tools"), "{refused}");
        let mute = catalogue.get("mute").unwrap();
        assert_eq!(mute.tools(crate::prompt::GRANTABLE_TOOLS).unwrap().len(), 0);
        assert!(mute.policy.refuse(tools::READ_FILE, &serde_json::json!({"path": "x"}).to_string()).is_some());
    }

    #[test]
    fn an_unknown_tool_is_refused_by_name() {
        let home = tempfile::tempdir().unwrap();
        profile(home.path(), "typo", "description: x\nallow_tools: [bsh]", "body");
        let catalogue = Catalogue::load(home.path());
        let error = catalogue.get("typo").unwrap().tools(crate::prompt::GRANTABLE_TOOLS).unwrap_err();
        assert!(error.contains("bsh") && error.contains("bash"), "{error}");
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
    fn a_profile_carries_its_flavour_model_and_grant() {
        let home = tempfile::tempdir().unwrap();
        profile(
            home.path(),
            "reviewer",
            "description: Reviews a diff.\nkeywords: [review, audit]\nmodel: main/coding\neffort: high\nread_only: true\nallow_tools: [read_file, bash]",
            "You review.",
        );
        let catalogue = Catalogue::load(home.path());
        let reviewer = catalogue.get("reviewer").unwrap();
        assert_eq!(reviewer.description, "Reviews a diff.");
        assert_eq!(reviewer.keywords, ["review", "audit"]);
        assert_eq!(reviewer.body, "You review.");
        assert_eq!(reviewer.model.as_deref(), Some("main/coding"));
        assert_eq!(reviewer.effort().unwrap(), Some("high"));
        assert_eq!(reviewer.role, Role::Worker);
        assert!(reviewer.policy.read_only);
        assert_eq!(reviewer.policy.allow_tools, ["read_file", "bash"]);
        assert!(
            catalogue
                .catalogue()
                .contains("- reviewer: Reviews a diff. (keywords: review, audit) [tools: read_file bash; read-only]"),
            "{}",
            catalogue.catalogue()
        );
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
        assert!(catalogue.read(ORCHESTRATOR).unwrap().contains("You own one task from the brief"));
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
