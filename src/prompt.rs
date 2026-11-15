use crate::mcp;
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

/// Name of the progress-note tool an agent writes to its owner with.
pub(crate) const REPLY: &str = "reply";

/// Every tool an owner may ban by name: the file tools, `reply`, and the tree tools.
const DENIABLE_TOOLS: &[&str] = &[
    tools::BASH,
    tools::READ_FILE,
    tools::WRITE_FILE,
    tools::EDIT,
    REPLY,
    SPAWN_AGENT,
    MESSAGE_AGENT,
    CONTROL_AGENT,
    LIST_AGENTS,
    LIST_REPOS,
];

/// Longest repository list `list_repos` shows.
pub(crate) const MAX_REPOS: usize = 300;

/// The tools an agent is offered: the file tools, `reply` and the tree tools, less what its policy
/// bans and what `remaining_depth` rules out, plus the servers' unless it is read-only.
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
    if !policy.read_only {
        functions.extend(mcp.tool_defs());
    }
    functions.retain(|tool| {
        let Some(name) = function_name(tool) else { return true };
        let banned = policy.deny_tools.iter().any(|tool| tool == name);
        let too_deep = name == SPAWN_AGENT && remaining_depth == 0;
        !banned && !too_deep
    });
    defs
}

/// The function name of one entry of the `tools` array.
fn function_name(tool: &Value) -> Option<&str> {
    tool.get("function")?.get("name")?.as_str()
}

/// The tools an agent commands the tree with, below the file tools.
fn swarm_tool_definitions() -> Vec<Value> {
    let spawn = with_policy(json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory the agent works in; relative paths resolve there. Defaults to your own directory."},
            "task": {"type": "string", "description": "Complete, self-contained instructions. The agent sees nothing else."}
        },
        "required": ["task"]
    }));
    vec![
        json!({"type": "function", "function": {
            "name": SPAWN_AGENT,
            "description": "Start a new agent below you, in a directory, under the rights you give it. Returns the agent id at once; its final answer arrives later as an [agent N finished] message. Its rights are fixed for its lifetime: deny `spawn_agent` in `deny_tools` and it can never start sub-agents. To change a running agent's rights, stop it (which stops everything below it) and spawn a new one.",
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
            "description": "Administer an agent you own. pause: it finishes the current instruction, then holds every queued one. resume: it takes work again. stop: its running turn is killed, it takes no more work, and everything below it is stopped too.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "action": {"type": "string", "enum": ["pause", "resume", "stop"]}
            }, "required": ["id", "action"]}
        }}),
        json!({"type": "function", "function": {
            "name": LIST_AGENTS,
            "description": "List the agents you own — yourself and everything below you — with depth, state, rights, directory and initial task.",
            "parameters": {"type": "object", "properties": {}}
        }}),
        json!({"type": "function", "function": {
            "name": LIST_REPOS,
            "description": "List the git repositories that already exist under the configured roots, so an agent can start in one. Never ask the user for a path.",
            "parameters": {"type": "object", "properties": {}}
        }}),
    ]
}

/// The restrictions `spawn_agent` may set, as the schema states them.
fn policy_properties() -> Value {
    json!({
        "read_only": {"type": "boolean", "description": format!("Refuse write_file, and refuse any bash command that is not a read-only program ({}) or a read-only git subcommand ({}); a writing flag or argument is refused too.", tools::READ_ONLY_PROGRAMS.join(" "), tools::READ_ONLY_GIT.join(" "))},
        "deny_tools": {"type": "array", "items": {"type": "string"}, "description": format!("Tools this agent may never call: {}. Denying {SPAWN_AGENT} stops it from ever starting sub-agents.", DENIABLE_TOOLS.join(", "))},
        "allow_commands": {"type": "array", "items": {"type": "string"}, "description": "When non-empty, a bash command's program word (a path and an `env`/`sudo` wrapper stripped, `VAR=x` skipped) must match one of these. A command carrying shell syntax (`;`, `|`, `&`, `>`) is refused, so this guards one program per call, not a shell."},
        "deny_commands": {"type": "array", "items": {"type": "string"}, "description": "A bash command whose program word matches one of these never runs, whatever its path or wrapper (`/bin/rm`, `env rm`). Deny beats allow."}
    })
}

/// A tool's parameters with the policy fields added.
fn with_policy(mut parameters: Value) -> Value {
    let Some(properties) = parameters.get_mut("properties").and_then(Value::as_object_mut) else {
        return parameters;
    };
    if let Some(policy) = policy_properties().as_object() {
        properties.extend(policy.clone());
    }
    parameters
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
    /// The agent the user talks to.
    pub(crate) root: bool,
}

pub(crate) fn agent_prompt(facts: &AgentFacts<'_>) -> String {
    let remaining = facts.max_depth.saturating_sub(facts.depth);
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
    out.push_str(facts.policy.describe().as_str());
    let catalogue = facts.skills.catalogue();
    if !catalogue.is_empty() {
        let _ = write!(
            out,
            "Skills you can read: when a task matches one, read `skill://<name>` with `read_file` first; `skill://<name>/<path>` reaches that skill's other files.\n<skills>\n{catalogue}\n</skills>\n"
        );
    }
    // A read-only agent is offered no MCP tool, so notes about them would describe calls it cannot
    // make. The servers' prose is fenced: it is theirs, not an instruction of yours.
    if !facts.policy.read_only {
        for (server, notes) in facts.mcp.instructions() {
            let _ = write!(out, "MCP server notes ({server}):\n<mcp server=\"{server}\">\n{notes}\n</mcp>\n");
        }
    }
    out.push_str(
        "Ownership is one way: you may message, pause, resume or stop any agent below you, and nobody below you may do any of that to you. You cannot command your parent, your siblings, or any agent outside your own subtree.\n\
Your rights are fixed: nothing can widen or narrow them while you run, and the same holds for the agents you own.\n\
Messages that start with [agent N ...] come from an agent you own: \"says\" is a progress note or a question, \"finished\" carries its final report, \"failed\" an error, \"interrupted\" a turn you cut short. Answer a question yourself when you can.\n\
Do the work yourself when it is yours to do; delegate when a task splits into independent parts, one agent per part, each with complete instructions because it sees nothing of your context except what you write. Parallel work in one repository means one worktree per agent on its own branch: `git worktree add ../<repo>-<slug> -b <type>/<slug> <base-branch>`. Two agents editing the same checkout is a dispatch bug, not a merge problem to fix later.\n\
Searching: `rg` (or `git grep`) scoped to the project, never a bare recursive grep over a code root — vendored trees (`.venv`, `node_modules`, `target`) make that take minutes.\n\
If you need a decision to continue, end your turn with the question as your final answer.\n",
    );
    let defs = agent_tool_definitions(facts.mcp, facts.policy, remaining);
    out.push_str(&tool_section(&defs));
    out.push_str(
        "Editing files: read a file with `read_file` before you change it — the numbers in that output are the anchors `edit` takes, and `edit` refuses a file this agent has not read or one that changed since. Use `edit` for every change to an existing file and `write_file` to create one. Do not rewrite source through `bash` (`sed -i`, `tee`, `>`, heredocs, `git apply`): those leave no record of what changed. A refusal telling you to read a range again means read that range, then edit.\n",
    );
    // The schemas of MCP tools are in the `tools` array already; the prompt only says where they are.
    let counts = facts.mcp.tool_counts();
    if !facts.policy.read_only && !counts.is_empty() {
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

    /// Facts for the root agent, on an empty home: no skill, no MCP server, the default policy.
    #[test]
    fn the_prompt_teaches_the_numbered_edit() {
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let policy = tools::Policy::default();
        let facts = AgentFacts {
            id: 1,
            parent: None,
            depth: 0,
            max_depth: 3,
            dir: home.path(),
            policy: &policy,
            children: Vec::new(),
            skills: &skills,
            mcp: &mcp,
            root: true,
        };
        let prompt = agent_prompt(&facts);
        assert!(prompt.contains("read a file with `read_file` before you change it"), "{prompt}");
        assert!(prompt.contains("Do not rewrite source through `bash`"), "{prompt}");
        assert!(prompt.contains("- edit: Change an existing file"), "{prompt}");
        assert!(prompt.contains("- read_file:"), "{prompt}");
    }
}
