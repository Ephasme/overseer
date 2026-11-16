use crate::mcp;
use crate::profiles;
use crate::rules;
use crate::skills;
use crate::tools;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::Path;

/// Names of the tree tools. The schema and the dispatch match both use these.
pub(crate) const SPAWN_AGENT: &str = "spawn_agent";
pub(crate) const MESSAGE_AGENT: &str = "message_agent";
pub(crate) const CONTROL_AGENT: &str = "control_agent";
pub(crate) const LIST_AGENTS: &str = "list_agents";
pub(crate) const LIST_REPOS: &str = "list_repos";
pub(crate) const START_PIPELINE: &str = "start_pipeline";
pub(crate) const LIST_PIPELINES: &str = "list_pipelines";
/// A grant, not a tool: it lets the holder pass `free: true` to `spawn_agent`.
pub(crate) const FREE_SPAWN: &str = "free_spawn";

/// The progress-note and closing tools live with the file tools: a policy refuses calls, and the
/// policy must not depend on this module.
pub(crate) use crate::tools::{CLOSE, REPLY};

/// Tools a profile may grant by name; `mcp__<server>` is accepted by `Profile::tools` separately.
/// `REPLY` and `CLOSE` are always granted, so listing them is allowed but unnecessary.
pub(crate) const GRANTABLE_TOOLS: &[&str] = &[
    tools::BASH,
    tools::READ_FILE,
    tools::WRITE_FILE,
    tools::EDIT,
    SPAWN_AGENT,
    MESSAGE_AGENT,
    CONTROL_AGENT,
    LIST_AGENTS,
    LIST_REPOS,
    START_PIPELINE,
    LIST_PIPELINES,
    FREE_SPAWN,
    tools::REPLY,
    tools::CLOSE,
];

/// Longest repository list `list_repos` shows.
pub(crate) const MAX_REPOS: usize = 300;

/// The tools an agent is offered: what its policy grants, less what `remaining_depth` rules out,
/// plus the servers' when it may call them.
///
/// The array must match what `Policy::refuse` and the dispatcher will run: a tool shown but
/// impossible to call teaches the model to waste turns on it.
pub(crate) fn agent_tool_definitions(mcp: &mcp::Registry, policy: &tools::Policy, remaining_depth: usize) -> Value {
    let mut defs = tools::definitions();
    let Some(functions) = defs.as_array_mut() else {
        return defs;
    };
    functions.push(json!({"type": "function", "function": {
        "name": REPLY,
        "description": "Send a short progress note to the agent that owns you, without ending your turn.",
        "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
    }}));
    functions.extend(swarm_tool_definitions());
    // A read-only agent is offered none of the MCP tools: their side effects are the server's,
    // not something the policy can predict.
    if !policy.read_only && policy.allow_tools.iter().any(|entry| entry.starts_with(mcp::PREFIX)) {
        functions.extend(mcp.tool_defs());
    }
    functions.retain(|tool| {
        let Some(name) = function_name(tool) else { return true };
        let always = tools::ALWAYS_GRANTED.contains(&name);
        // Both start a child, so neither is offered to an agent that may have none.
        let too_deep = (name == SPAWN_AGENT || name == START_PIPELINE) && remaining_depth == 0;
        (always || tools::grants(&policy.allow_tools, name)) && !too_deep
    });
    defs
}

/// The function name of one entry of the `tools` array.
fn function_name(tool: &Value) -> Option<&str> {
    tool.get("function")?.get("name")?.as_str()
}

/// The names of every tool this caller is offered, in the order the `tools` array lists them.
pub(crate) fn tool_names(mcp: &mcp::Registry, policy: &tools::Policy, remaining_depth: usize) -> Vec<String> {
    agent_tool_definitions(mcp, policy, remaining_depth)
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| function_name(tool).map(ToString::to_string))
        .collect()
}

/// One line per tool: its name and the first line of its description, for `overseer://tools`.
pub(crate) fn tool_summaries(mcp: &mcp::Registry, policy: &tools::Policy, remaining_depth: usize) -> Vec<String> {
    agent_tool_definitions(mcp, policy, remaining_depth)
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?;
            let description = function.get("description").and_then(Value::as_str).unwrap_or_default();
            Some(format!("{name}: {}", description.lines().next().unwrap_or_default()))
        })
        .collect()
}

/// One tool's schema as pretty JSON, or why no tool of this caller answers to the name.
pub(crate) fn schema(
    name: &str,
    mcp: &mcp::Registry,
    policy: &tools::Policy,
    remaining_depth: usize,
) -> Result<String, String> {
    let defs = agent_tool_definitions(mcp, policy, remaining_depth);
    let found = defs.as_array().into_iter().flatten().find(|tool| function_name(tool) == Some(name));
    let Some(tool) = found else {
        return Err(format!("unknown tool {name}; tools are {}", tool_names(mcp, policy, remaining_depth).join(", ")));
    };
    let function = tool.get("function").cloned().unwrap_or_else(|| tool.clone());
    serde_json::to_string_pretty(&function).map_err(|e| format!("{name}: {e}"))
}

/// The tools an agent commands the tree with, and the two that end its own turn.
fn swarm_tool_definitions() -> Vec<Value> {
    let spawn = json!({
        "type": "object",
        "properties": {
            "profile": {"type": "string", "description": "The profile the new agent runs, from the catalogue your prompt lists. It fixes its model, its effort and its rights."},
            "path": {"type": "string", "description": "Directory the agent works in; relative paths resolve there. Defaults to your own directory."},
            "task": {"type": "string", "description": "Complete, self-contained instructions. The agent sees nothing else."},
            "free": {"type": "boolean", "description": "Spawn with the child profile's policy instead of the intersection with your own. Requires the free_spawn grant."}
        },
        "required": ["task", "profile"]
    });
    vec![
        json!({"type": "function", "function": {
            "name": SPAWN_AGENT,
            "description": "Start a new agent below you, in a directory, with a profile from the catalogue. The profile fixes its model, its effort and its rights; the call cannot add or remove one. Returns the agent id at once; its final answer arrives later as an [agent N · <name> finished] message. To change a running agent's rights, stop it (which stops everything below it) and spawn a new one.",
            "parameters": spawn
        }}),
        json!({"type": "function", "function": {
            "name": MESSAGE_AGENT,
            "description": "Send a follow-up instruction to an agent you own. It keeps its history; a busy agent reads it after its current instruction unless the message is urgent.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "text": {"type": "string"},
                "urgent": {"type": "boolean", "description": "Cut the running turn short and deliver this now, ahead of anything queued. Its running command is killed; a model call already in flight finishes first."}
            }, "required": ["id", "text"]}
        }}),
        json!({"type": "function", "function": {
            "name": CONTROL_AGENT,
            "description": "Administer an agent you own. pause: it finishes the current instruction, then holds every queued one. resume: it takes work again. stop: its running turn is killed, it takes no more work, and everything below it is stopped too. archive: a settled agent leaves the user's view; its entry and log stay.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "action": {"type": "string", "enum": ["pause", "resume", "stop", "archive"]}
            }, "required": ["id", "action"]}
        }}),
        json!({"type": "function", "function": {
            "name": LIST_AGENTS,
            "description": "List the agents you own — yourself and everything below you — with depth, state, rights, profile, directory and initial task.",
            "parameters": {"type": "object", "properties": {}}
        }}),
        json!({"type": "function", "function": {
            "name": LIST_REPOS,
            "description": "List the git repositories that already exist under the configured roots, so an agent can start in one. Never ask the user for a path.",
            "parameters": {"type": "object", "properties": {}}
        }}),
        json!({"type": "function", "function": {
            "name": START_PIPELINE,
            "description": "Start one of the user's Rhai pipelines as a child agent, in your own directory. The script runs under a fixed policy and reports through its host functions; a tool its header requires but that policy refuses stops the call before anything is created.",
            "parameters": {"type": "object", "properties": {
                "name": {"type": "string", "description": "The pipeline name, from the catalogue your prompt lists."}
            }, "required": ["name"]}
        }}),
        json!({"type": "function", "function": {
            "name": LIST_PIPELINES,
            "description": "List the pipeline names the user's catalogue holds.",
            "parameters": {"type": "object", "properties": {}}
        }}),
        json!({"type": "function", "function": {
            "name": CLOSE,
            "description": "End your turn with your final report and close yourself. Your owner re-opens you by sending another instruction; only your owner can archive you.",
            "parameters": {"type": "object", "properties": {"report": {"type": "string", "description": "The complete report for your owner."}}, "required": ["report"]}
        }}),
    ]
}

/// What one agent is told about itself and its place in the tree.
pub(crate) struct AgentFacts<'a> {
    pub(crate) id: usize,
    pub(crate) parent: Option<usize>,
    pub(crate) depth: usize,
    pub(crate) max_depth: usize,
    pub(crate) dir: &'a Path,
    pub(crate) policy: &'a tools::Policy,
    /// Agents it owns right now.
    pub(crate) children: Vec<usize>,
    /// The skills it may read, and the servers whose tools it may call.
    pub(crate) skills: &'a skills::Catalogue,
    pub(crate) mcp: &'a mcp::Registry,
    /// The flavor this agent runs, when its spawner named one.
    pub(crate) profile: Option<&'a profiles::Profile>,
    /// Every profile a spawner may pick from.
    pub(crate) profiles: &'a profiles::Catalogue,
    /// Whether it runs the tree or does the work.
    pub(crate) role: profiles::Role,
    /// The agent the user talks to.
    pub(crate) root: bool,
    /// The instruction files loaded at thread start, carried by every prompt rebuild.
    pub(crate) rules: &'a rules::Rules,
}

/// What a supervisor is for. It never changes anything itself: every change belongs to an agent it
/// starts, and its own instruments are the tree tools, `read_file` and read-only commands.
const SUPERVISOR_PROMPT: &str = "You are the supervisor. You never change anything: you do not write files, you do not run a command that changes anything, and you do not merge. Everything that changes the repository belongs to an agent you start. Your instruments are spawn_agent, message_agent, control_agent, list_agents, list_repos, start_pipeline, read_file and read-only commands. Answering a question from what you can read is your job too: do not spawn an agent to tell you what a file says.\n\
A request that needs work is not answered, it is run. One task gets one orchestrator: spawn an `orchestrator` with the request rewritten as a complete brief — goal, directory, base branch, constraints, what \"done\" means, which profiles it may use, and the evidence you will demand. The orchestrator owns the execution; you own the outcome.\n\
The loop:\n\
1. Brief before spawn. Never send a brief you would not accept as an outsider: no context the agent cannot read, no branch it must guess, no acceptance you cannot check.\n\
2. Plan before code. No worker starts before a written plan exists at a path you can read. Read it yourself and attack it: vague slices, claims about the code nobody verified, a slice with no test, a \"done\" with no command.\n\
3. Second opinion. For every contested, irreversible or expensive decision, spawn two `researcher` agents at the same time, the same question asked from two angles (the code as it is; the ecosystem, the docs, the prior art). Compare; where they disagree, get a third or make the orchestrator settle it with evidence. Decide, and tell the orchestrator why.\n\
4. Push. list_agents before every message: idle with nothing queued means stopped or closed, not finished. Short specific objections, not restatements. `urgent` only to cut a turn that is heading wrong — it kills the running command.\n\
5. Challenge every \"done\". Which commands ran, what did they print, which files changed, what is still untested. A claim without a command and its output is not evidence.\n\
6. Review every build. A slice is reviewed by a `build-reviewer` that did not write it, and a report that names neither the commands nor their result is sent back.\n\
7. Integrate through an agent. The integrator merges in dependency order, rebases between merges, and leaves the base green. You verify with read-only commands and read the diff; you never merge, rebase or commit.\n\
8. Archive. When an agent has closed or stopped and you have read its report, archive it with control_agent `archive`. Its entry and log stay; only the user's default view forgets it.\n\
9. Answer the user. What is now true, the evidence that proves it, what remains open, and each decision you took on their behalf. Your agents' reports are not the answer; your judgement is.\n\
A wrong report is a briefing failure first: say what you now know and re-task. Stop what is redundant; pause what you are about to correct. A closed agent you need again is re-opened by sending it an instruction. If you need a decision from the user, end your turn with that one question.\n";

/// What a worker is for: the task in its instruction, and a report its owner can act on.
const WORKER_PROMPT: &str = "You do the job your instruction names, and you answer for it. A report is what your owner reads: say what you changed, the commands you ran and what they printed, what you checked, and what remains untested. If your profile grants spawn_agent and the task splits into parts one agent can own alone, delegate those parts with complete instructions; otherwise do the work. Call `close` with your report as your last tool call when the task is finished: your owner can re-open you, and only your owner can archive you.\n";

pub(crate) fn agent_prompt(facts: &AgentFacts<'_>) -> String {
    let remaining = facts.max_depth.saturating_sub(facts.depth);
    // What the agent may actually call: a supervisor's role narrows its profile before anything is
    // offered, so it never reads about a tool it cannot use.
    let policy = facts.role.restrict(facts.policy);
    let mut out = format!(
        "You are agent {} of a tree of identical agents, working in {}; relative paths resolve there.\n\
Depth: you are at depth {} and agents may be nested to depth {} (config `max_depth`), so you can start {} more level(s) of agents with `spawn_agent`.\n",
        facts.id,
        facts.dir.display(),
        facts.depth,
        facts.max_depth,
        remaining
    );
    if facts.root {
        out.push_str("The user talks to you directly; your final answer is what they read.\n");
    } else if let Some(parent) = facts.parent {
        let _ = writeln!(
            out,
            "Agent {parent} started you and owns you: your final answer goes to it, so make it a complete report."
        );
    } else {
        // Non-root with no parent: the caller contradicted itself, and naming agent 0 would give the
        // model an owner it does not have.
        out.push_str("Your owner is not recorded: this agent reported itself as non-root with no parent.\n");
    }
    if !facts.children.is_empty() {
        let listed: Vec<String> = facts.children.iter().map(|id| format!("agent {id}")).collect();
        let _ = writeln!(out, "Agents you own right now: {}.", listed.join(", "));
    }
    out.push_str(match facts.role {
        profiles::Role::Supervisor => SUPERVISOR_PROMPT,
        profiles::Role::Worker => WORKER_PROMPT,
    });
    if let Some(profile) = facts.profile {
        let _ = writeln!(out, "Profile {}: {}", profile.name, profile.body);
    }
    out.push_str(facts.role.describe(&policy).as_str());
    let may_spawn = remaining > 0 && tools::grants(&policy.allow_tools, SPAWN_AGENT);
    if may_spawn {
        let _ = write!(
            out,
            "Profiles for your sub-agents: pass `profile` to `spawn_agent`, or read `profile://<name>` for the full file. A child gets the intersection of your tool list and its profile's, unless you hold the free_spawn grant and pass `free: true`, which gives it its profile's rights.\n<profiles>\n{}\n</profiles>\n",
            facts.profiles.catalogue()
        );
    }
    let catalogue = facts.skills.catalogue();
    if !catalogue.is_empty() {
        let _ = write!(
            out,
            "Skills you can read: when a task matches one, read `skill://<name>` with `read_file` first; `skill://<name>/<path>` reaches that skill's other files.\n<skills>\n{catalogue}\n</skills>\n"
        );
    }
    if !facts.rules.is_empty() {
        out.push_str(&rules_section(facts.rules));
    }
    // A read-only agent is offered no MCP tool, so notes about them would describe calls it cannot
    // make. The servers' prose is fenced: it is theirs, not an instruction of yours.
    if !policy.read_only {
        for (server, notes) in facts.mcp.instructions() {
            let _ = write!(out, "MCP server notes ({server}):\n<mcp server=\"{server}\">\n{notes}\n</mcp>\n");
        }
    }
    out.push_str(
        "Ownership is one way: you may message, pause, resume or stop any agent below you, and nobody below you may do any of that to you. You cannot command your parent, your siblings, or any agent outside your own subtree.\n\
Your rights are fixed: nothing can widen or narrow them while you run, and the same holds for the agents you own.\n\
Messages that start with [agent N · <name> ...] come from an agent you own, and a renamed agent shows its new name there: \"says\" is a progress note or a question, \"finished\" carries its final report, \"failed\" an error, \"interrupted\" a turn you cut short. Answer a question yourself when you can.\n\
Searching: `rg` (or `git grep`) scoped to the project, never a bare recursive grep over a code root — vendored trees (`.venv`, `node_modules`, `target`) make that take minutes.\n\
If you need a decision to continue, end your turn with the question as your final answer.\n",
    );
    let defs = agent_tool_definitions(facts.mcp, &policy, remaining);
    out.push_str(&tool_section(&defs));
    if tools::grants(&policy.allow_tools, tools::EDIT) {
        out.push_str(
            "Editing files: read a file with `read_file` before you change it — the numbers in that output are the anchors `edit` takes, and `edit` refuses a file this agent has not read or one that changed since. Use `edit` for every change to an existing file and `write_file` to create one. Do not rewrite source through `bash` (`sed -i`, `tee`, `>`, heredocs, `git apply`): those leave no record of what changed. A refusal telling you to read a range again means read that range, then edit.\n",
        );
    }
    // The schemas of MCP tools are in the `tools` array already; the prompt only says where they are.
    let counts = facts.mcp.tool_counts();
    if !policy.read_only && !counts.is_empty() {
        let lines: Vec<String> = counts
            .iter()
            .map(|(server, count)| {
                format!("MCP tools on {server}: {count} (mcp__<server>_<tool>; schemas are in the tools array)")
            })
            .collect();
        out.push_str(&lines.join("\n"));
        out.push('\n');
    }
    out
}

/// The loaded instruction files, in discovery order, keeping loaded files under a 64 KiB budget that
/// counts headers, content and separators; a file past the budget, or one a read error skipped, gets
/// one pointer line instead, and the pointer lines and closing tag are not counted against it.
fn rules_section(r: &rules::Rules) -> String {
    const BUDGET: usize = 64 * 1024;
    let mut out = String::from(
        "Instruction files that govern your working directory: follow them. `rule://` lists every \
         rule file, `rule://<path>` reads one.\n\n<rules>\n",
    );
    for (path, loaded) in r.iter() {
        let header = format!("# {}\n", path.display());
        let Some((text, cut)) = loaded else {
            let _ = writeln!(out, "[not loaded: {} — read rule://{}]", path.display(), path.display());
            continue;
        };
        if out.len() + header.len() + text.len() > BUDGET {
            let _ = writeln!(out, "[not loaded: {} — read rule://{}]", path.display(), path.display());
            continue;
        }
        out.push_str(&header);
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
        if *cut {
            let _ = writeln!(
                out,
                "[truncated: the first {} bytes only; rule://{} returns the same head, so read the rest with bash]",
                text.len(),
                path.display()
            );
        }
    }
    out.push_str("</rules>\n");
    out
}

/// Renders tool names and descriptions into prompt lines; the `tools` array carries the schemas.
fn tool_section(defs: &Value) -> String {
    let lines: Vec<String> = defs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?;
            // MCP tools are summarised per server by the caller: one line each would drown the prompt.
            if name.starts_with(mcp::PREFIX) {
                return None;
            }
            let description = function.get("description").and_then(Value::as_str).unwrap_or_default();
            Some(format!("- {name}: {description}"))
        })
        .collect();
    format!("Tools:\n{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_profile_appends_its_flavor_and_the_catalogue_to_the_base_prompt() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".overseer/agent/profiles");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("reviewer.md"),
            "---\ndescription: Reviews a diff.\nkeywords: [review, audit]\nread_only: true\nallow_tools: [read_file]\n---\nYou review. Never edit.\n",
        )
        .unwrap();
        let profiles = profiles::Catalogue::load(home.path());
        let policy =
            tools::Policy::default().set(None, Some(vec![tools::READ_FILE.into(), SPAWN_AGENT.into()]), None, None);
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let mcp = mcp::Registry::load(home.path());
        let reviewer = profiles.get("reviewer").unwrap();
        let facts = AgentFacts {
            id: 2,
            parent: Some(1),
            depth: 1,
            max_depth: 3,
            dir: home.path(),
            policy: &policy,
            children: Vec::new(),
            skills: &skills,
            rules: &rules::Rules::load(Path::new("/nonexistent"), home.path()),
            mcp: &mcp,
            root: false,
            profile: Some(reviewer),
            profiles: &profiles,
            role: profiles::Role::Worker,
        };
        let prompt = agent_prompt(&facts);
        assert!(prompt.contains("You are agent 2 of a tree of identical agents"), "the base prompt stays");
        assert!(prompt.contains("Profile reviewer: You review. Never edit."), "{prompt}");
        assert!(
            prompt.contains("- reviewer: Reviews a diff. (keywords: review, audit) [tools: read_file; read-only]"),
            "{prompt}"
        );
        assert!(prompt.contains("profile://<name>"), "the detail path is named: {prompt}");
        assert!(prompt.contains("- tools you may call: read_file spawn_agent."), "the grant is stated: {prompt}");
    }

    #[test]
    fn an_agent_that_cannot_spawn_reads_no_catalogue() {
        let home = tempfile::tempdir().unwrap();
        let profiles = profiles::Catalogue::default();
        let policy = tools::Policy::default().set(None, Some(vec![tools::READ_FILE.into()]), None, None);
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let mcp = mcp::Registry::load(home.path());
        let facts = AgentFacts {
            id: 2,
            parent: Some(1),
            depth: 1,
            max_depth: 3,
            dir: home.path(),
            policy: &policy,
            children: Vec::new(),
            skills: &skills,
            rules: &rules::Rules::load(Path::new("/nonexistent"), home.path()),
            mcp: &mcp,
            root: false,
            profile: None,
            profiles: &profiles,
            role: profiles::Role::Worker,
        };
        let prompt = agent_prompt(&facts);
        assert!(!prompt.contains("Profiles for your sub-agents"), "{prompt}");
        assert!(!prompt.contains("<profiles>"), "{prompt}");
    }

    #[test]
    fn an_empty_allow_list_offers_only_reply_and_close() {
        let mcp = mcp::Registry::load(Path::new("/nonexistent"));
        let policy = tools::Policy::default();
        let names: Vec<String> = agent_tool_definitions(&mcp, &policy, 3)
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("function")?.get("name")?.as_str().map(str::to_string))
            .collect();
        assert_eq!(names, [REPLY, CLOSE]);
    }

    #[test]
    fn the_supervisor_prompt_forbids_changing_anything() {
        let home = tempfile::tempdir().unwrap();
        let profiles = profiles::Catalogue::default();
        let policy = profiles.get(profiles::SUPERVISOR).unwrap().policy.clone();
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let mcp = mcp::Registry::load(home.path());
        let facts = AgentFacts {
            id: 1,
            parent: None,
            depth: 0,
            max_depth: 3,
            dir: home.path(),
            policy: &policy,
            children: Vec::new(),
            skills: &skills,
            rules: &rules::Rules::load(Path::new("/nonexistent"), home.path()),
            mcp: &mcp,
            root: true,
            profile: profiles.get(profiles::SUPERVISOR),
            profiles: &profiles,
            role: profiles::Role::Supervisor,
        };
        let prompt = agent_prompt(&facts);
        assert!(prompt.contains("You are the supervisor"), "{prompt}");
        assert!(prompt.contains("You never change anything"), "{prompt}");
        assert!(!prompt.contains("Do the work yourself"), "{prompt}");
        assert!(!prompt.contains("- write_file:"), "the tool is not offered: {prompt}");
    }

    /// The edit paragraph reaches an agent that may call the tool, and the tool list names it.
    #[test]
    fn the_prompt_teaches_the_numbered_edit() {
        let home = tempfile::tempdir().unwrap();
        let profiles = profiles::Catalogue::default();
        let policy = tools::Policy {
            allow_tools: vec![tools::READ_FILE.into(), tools::WRITE_FILE.into(), tools::EDIT.into()],
            ..tools::Policy::default()
        };
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let mcp = mcp::Registry::load(home.path());
        let facts = AgentFacts {
            id: 2,
            parent: Some(1),
            depth: 1,
            max_depth: 3,
            dir: home.path(),
            policy: &policy,
            children: Vec::new(),
            skills: &skills,
            rules: &rules::Rules::load(Path::new("/nonexistent"), home.path()),
            mcp: &mcp,
            root: false,
            profile: None,
            profiles: &profiles,
            role: profiles::Role::Worker,
        };
        let prompt = agent_prompt(&facts);
        assert!(prompt.contains("read a file with `read_file` before you change it"), "{prompt}");
        assert!(prompt.contains("Do not rewrite source through `bash`"), "{prompt}");
        assert!(prompt.contains("- edit: Change an existing file"), "{prompt}");
        assert!(prompt.contains("- read_file:"), "{prompt}");
    }

    /// A minimal facts bundle over an empty rules cache, so a test names only what it varies.
    fn facts_with<'a>(
        home: &'a Path,
        policy: &'a tools::Policy,
        skills: &'a skills::Catalogue,
        mcp: &'a mcp::Registry,
        profiles: &'a profiles::Catalogue,
        rules: &'a rules::Rules,
    ) -> AgentFacts<'a> {
        AgentFacts {
            id: 2,
            parent: Some(1),
            depth: 1,
            max_depth: 3,
            dir: home,
            policy,
            children: Vec::new(),
            skills,
            rules,
            mcp,
            root: false,
            profile: None,
            profiles,
            role: profiles::Role::Worker,
        }
    }

    #[test]
    fn loaded_rules_reach_the_prompt_and_empty_ones_do_not() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("AGENTS.md"), "be brief").unwrap();
        let rules = rules::Rules::load(&home.path().join("config"), home.path());
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let mcp = mcp::Registry::load(home.path());
        let profiles = profiles::Catalogue::default();
        let policy = tools::Policy::default();
        let prompt = agent_prompt(&facts_with(home.path(), &policy, &skills, &mcp, &profiles, &rules));
        assert!(prompt.contains("<rules>"), "{prompt}");
        let header = format!("# {}", home.path().canonicalize().unwrap().join("AGENTS.md").display());
        assert!(prompt.contains(&header), "{prompt}");
        assert!(prompt.contains("be brief"), "{prompt}");

        let bare = tempfile::tempdir().unwrap();
        fs::create_dir(bare.path().join(".git")).unwrap();
        let empty = rules::Rules::load(&bare.path().join("config"), bare.path());
        let prompt = agent_prompt(&facts_with(bare.path(), &policy, &skills, &mcp, &profiles, &empty));
        assert!(!prompt.contains("<rules>"), "{prompt}");
    }

    #[test]
    fn the_rules_budget_drops_whole_files_and_names_them() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().canonicalize().unwrap();
        fs::create_dir(base.join(".git")).unwrap();
        for (name, size) in [("AGENTS.md", 25_000), ("CLAUDE.md", 25_000), ("RULES.md", 25_000)] {
            fs::write(base.join(name), "x".repeat(size)).unwrap();
        }
        let rules = rules::Rules::load(&base.join("config"), &base);
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let mcp = mcp::Registry::load(home.path());
        let profiles = profiles::Catalogue::default();
        let policy = tools::Policy::default();
        let prompt = agent_prompt(&facts_with(home.path(), &policy, &skills, &mcp, &profiles, &rules));
        let a = base.join("AGENTS.md").display().to_string();
        let c = base.join("RULES.md").display().to_string();
        assert!(prompt.contains(&format!("# {a}")), "the first file fits: {prompt}");
        assert!(!prompt.contains(&format!("# {c}")), "the third file is over budget");
        assert!(prompt.contains(&format!("[not loaded: {c} — read rule://{c}]")), "{prompt}");
    }

    #[test]
    fn an_oversized_rule_file_keeps_its_head_and_names_the_rest() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().canonicalize().unwrap();
        fs::write(base.join("AGENTS.md"), "y".repeat(30_000)).unwrap();
        fs::create_dir(base.join(".git")).unwrap();
        let rules = rules::Rules::load(&base.join("config"), &base);
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let mcp = mcp::Registry::load(home.path());
        let profiles = profiles::Catalogue::default();
        let policy = tools::Policy::default();
        let prompt = agent_prompt(&facts_with(home.path(), &policy, &skills, &mcp, &profiles, &rules));
        let path = base.join("AGENTS.md").display().to_string();
        assert!(prompt.contains("yyyy"), "the head is carried: {prompt}");
        assert!(prompt.contains(&format!("[truncated: the first 28000 bytes only; rule://{path}")), "{prompt}");
    }
}
