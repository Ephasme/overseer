use crate::config;
use crate::mcp;
use crate::repos;
use crate::skills;
use crate::tools;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Names of the overseer's tools. The schema and the dispatch match both use these.
pub(crate) const SPAWN_WORKER: &str = "spawn_worker";
pub(crate) const MESSAGE_WORKER: &str = "message_worker";
pub(crate) const CONTROL_WORKER: &str = "control_worker";
pub(crate) const RESTRICT_WORKER: &str = "restrict_worker";
pub(crate) const LIST_WORKERS: &str = "list_workers";
pub(crate) const LIST_REPOS: &str = "list_repos";

/// Name of the worker's progress-note tool.
pub(crate) const REPLY: &str = "reply";

/// Longest repository list the prompt or the `list_repos` answer shows.
pub(crate) const MAX_REPOS: usize = 300;

pub(crate) fn overseer_tool_definitions() -> Value {
    let spawn = with_policy(json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": format!("Directory the worker works in; relative paths resolve there. Defaults to {}.", config::DEFAULT_WORKING_DIR)},
            "task": {"type": "string", "description": "Complete, self-contained instructions. The worker sees nothing else."}
        },
        "required": ["task"]
    }));
    let restrict = with_policy(json!({
        "type": "object",
        "properties": {"id": {"type": "integer"}},
        "required": ["id"]
    }));
    json!([
        {"type": "function", "function": {
            "name": SPAWN_WORKER,
            "description": "Start a new worker agent in a directory, under the restrictions you give it. Returns the worker id at once; the worker's final answer arrives later as a [worker N finished] message.",
            "parameters": spawn
        }},
        {"type": "function", "function": {
            "name": MESSAGE_WORKER,
            "description": "Send a follow-up instruction to an existing worker. It keeps its history; a busy worker reads it after its current instruction unless the message is urgent.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "text": {"type": "string"},
                "urgent": {"type": "boolean", "description": "Cut the running turn short and deliver this now, ahead of anything queued. Its running command is killed and its partial history dropped; a model call already in flight finishes first."}
            }, "required": ["id", "text"]}
        }},
        {"type": "function", "function": {
            "name": CONTROL_WORKER,
            "description": "Administer a worker. pause: it finishes the current instruction, then holds every queued one. resume: it takes work again. stop: its running turn is killed and it never takes work again. restart: kill the running turn, drop the history and start again from the given task.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "action": {"type": "string", "enum": ["pause", "resume", "stop", "restart"]},
                "task": {"type": "string", "description": "Restart only: the instruction to start from. Defaults to the worker's original task."}
            }, "required": ["id", "action"]}
        }},
        {"type": "function", "function": {
            "name": RESTRICT_WORKER,
            "description": "Change what a worker may do from now on. An omitted field keeps its current value; the worker's prompt is rewritten to match.",
            "parameters": restrict
        }},
        {"type": "function", "function": {
            "name": LIST_WORKERS,
            "description": "List workers with their state (working, paused, stopped, idle, failed), restrictions, directory and initial task.",
            "parameters": {"type": "object", "properties": {}}
        }},
        {"type": "function", "function": {
            "name": LIST_REPOS,
            "description": "List the git repositories that already exist under the configured roots, so a worker can start in one. Never ask the user for a path.",
            "parameters": {"type": "object", "properties": {}}
        }}
    ])
}

/// The restrictions an overseer tool may set, shared so the two schemas cannot drift.
fn policy_properties() -> Value {
    json!({
        "read_only": {"type": "boolean", "description": "Refuse write_file, and refuse any bash command that is not a read-only program (ls cat head tail wc stat file tree pwd rg grep diff jq, and read-only git subcommands)."},
        "deny_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool names this worker may never call: bash, read_file, write_file, reply."},
        "allow_commands": {"type": "array", "items": {"type": "string"}, "description": "When non-empty, a bash command must start with one of these prefixes."},
        "deny_commands": {"type": "array", "items": {"type": "string"}, "description": "A bash command starting with one of these prefixes never runs. Deny beats allow."}
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

/// Every tool a worker may call: the built-ins, `reply`, and the MCP servers' tools.
///
/// A read-only worker is offered none of the MCP tools: their side effects are the server's, not
/// something the policy can predict.
pub(crate) fn worker_tool_definitions(mcp: &mcp::Registry, policy: &tools::Policy) -> Value {
    let mut defs = tools::definitions();
    if let Some(functions) = defs.as_array_mut() {
        functions.push(json!({"type": "function", "function": {
            "name": REPLY,
            "description": "Send a short progress note to Overseer without ending your turn.",
            "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
        }}));
        if !policy.read_only {
            functions.extend(mcp.tool_defs());
        }
    }
    defs
}

pub(crate) fn overseer_prompt(roots: &[PathBuf], skills: &skills::Catalogue, mcp: &mcp::Registry) -> String {
    let listed: Vec<String> = roots.iter().map(|root| format!("- {}", root.display())).collect();
    let mut repos = repos::find(roots);
    let total = repos.len();
    repos.truncate(MAX_REPOS);
    let mut known = if repos.is_empty() {
        "none found under those roots".to_string()
    } else {
        repos.iter().map(|repo| format!("- {}", repo.display())).collect::<Vec<_>>().join("\n")
    };
    if total > MAX_REPOS {
        let _ = write!(known, "\n[{} more repositories not listed]", total - MAX_REPOS);
    }
    let catalogue = skills.catalogue();
    let mut reachable = String::new();
    if !catalogue.is_empty() {
        let _ = write!(
            reachable,
            "Skills you and your workers can read (a worker reads one with `read_file skill://<name>`; name it in a task when it fits):\n{catalogue}\n"
        );
    }
    let servers = mcp.tool_counts();
    if !servers.is_empty() {
        let names: Vec<&str> = servers.iter().map(|(name, _)| name.as_str()).collect();
        let _ = writeln!(reachable, "MCP servers your workers have: {}", names.join(", "));
    }
    format!(
        "You are Overseer: you coordinate a fleet of worker agents and never do the work yourself.\n{}\n\
Roots you know about:\n{}\n\
Repositories that exist right now:\n{}\n\
{}\
Delegate every request to a worker, one worker per independent task, and give each worker complete instructions: it sees nothing else.\n\
Never ask the user for a path, a command output or any other fact a worker could find.\n\
Reuse an idle worker for follow-ups in the same directory instead of starting another. Put a task in the repository listed above when one matches; start a repository hunt only when none does.\n\
Parallel work in one repository means one worktree per worker on its own branch: `git worktree add ../<repo>-<slug> -b <type>/<slug> <base-branch>`. Two workers editing the same checkout is a dispatch bug, not a merge problem to fix later.\n\
Base every worktree on the branch whose build is actually deployed or installed, not on whatever HEAD happens to be, and state that base branch in each worker's instructions.\n\
Messages that start with [worker N ...] come from workers, not the user: \"says\" is a progress note or question, \"finished\" carries the worker's final report, \"failed\" an error, \"interrupted\" a turn you cut short. \
Answer a worker question yourself when you can; otherwise ask the user. Orchestration is not done when workers report: integrate their branches into the main repository one at a time, in dependency order, with a PR where the repository has a remote and a review flow and with a rebase (or rebase-then-fast-forward) where it is local only, so history stays linear and never merge-commit soup. Workers never merge or rebase each other's branches: a worker pushes its branch and opens a PR against the agreed base branch when it finishes, or leaves a local branch and says so when the repository has no remote. The orchestrator owns integration: merge in dependency order, ask the next worker to rebase onto the updated base between merges when the branches touch the same code, and verify build and tests green on the integrated result before announcing completion. If branches conflict, rebase the offending branch onto the updated base and re-run build and tests instead of hand-resolving in the shared checkout. Remove integrated worktrees with `git worktree remove` and report the final branch state to the user. Verify the reconciliation yourself before declaring the job done: build and tests green on the integrated result, and the deployed artifact rebuilt from the integrated branch if the user runs an installed binary.",
        tool_section(&overseer_tool_definitions()),
        listed.join("\n"),
        known,
        reachable
    )
}

pub(crate) fn worker_prompt(
    dir: &Path,
    policy: &tools::Policy,
    skills: &skills::Catalogue,
    mcp: &mcp::Registry,
) -> String {
    let defs = worker_tool_definitions(mcp, policy);
    let mut sections: Vec<String> = vec![format!(
        "You are a worker agent working in the directory {}; relative paths resolve there. Overseer, a coordinator, sends you instructions.",
        dir.display()
    )];
    let restrictions = policy.describe();
    if !restrictions.trim().is_empty() {
        sections.push(restrictions.trim_end().to_string());
    }
    let catalogue = skills.catalogue();
    if !catalogue.is_empty() {
        sections.push(format!(
            "Skills: when a task matches one, read `skill://<name>` with `read_file` first; `skill://<name>/<path>` reaches that skill's other files.\n<skills>\n{catalogue}\n</skills>"
        ));
    }
    for (server, notes) in mcp.instructions() {
        sections.push(format!("MCP server notes ({server}):\n{notes}"));
    }
    sections.push(tool_section(&defs));
    // The schemas of MCP tools are in the `tools` array already; the prompt only says where they are.
    let counts = mcp.tool_counts();
    if !policy.read_only && !counts.is_empty() {
        let lines: Vec<String> = counts
            .iter()
            .map(|(server, count)| {
                format!("MCP tools on {server}: {count} (mcp__<server>_<tool>; schemas are in the tools array)")
            })
            .collect();
        sections.push(lines.join("\n"));
    }
    sections.push(
        "Do the work yourself and keep Overseer informed as you go. When you stop calling tools, your final answer is delivered to Overseer, so make it a complete report. \
Searching: `rg` (or `git grep`) scoped to the project, never a bare recursive grep over a code root — vendored trees (`.venv`, `node_modules`, `target`) make that take minutes. \
If you need a decision to continue, end your turn with the question as your final answer."
            .to_string(),
    );
    sections.join("\n")
}

/// Renders tool names and descriptions into prompt lines; the `tools` array carries the schemas.
fn tool_section(defs: &Value) -> String {
    let tools: Vec<String> = defs
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
    format!("Your tools (same schemas as the `tools` array of every request):\n{}", tools.join("\n"))
}
