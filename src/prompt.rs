use crate::repos;
use crate::tools;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Names of the overseer's tools. The schema and the dispatch match both use these.
pub(crate) const SPAWN_WORKER: &str = "spawn_worker";
pub(crate) const MESSAGE_WORKER: &str = "message_worker";
pub(crate) const LIST_WORKERS: &str = "list_workers";
pub(crate) const LIST_REPOS: &str = "list_repos";

/// Name of the worker's progress-note tool.
pub(crate) const REPLY: &str = "reply";

/// Where a worker starts when `spawn_worker` names no directory.
pub(crate) const DEFAULT_WORKING_DIR: &str = "/tmp";

/// Longest repository list the prompt or the `list_repos` answer shows.
pub(crate) const MAX_REPOS: usize = 300;

pub(crate) fn overseer_tool_definitions() -> Value {
    json!([
        {"type": "function", "function": {
            "name": SPAWN_WORKER,
            "description": "Start a new worker agent in a directory. Returns the worker id at once; the worker's final answer arrives later as a [worker N finished] message.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": format!("Directory the worker works in; relative paths resolve there. Defaults to {DEFAULT_WORKING_DIR}.")},
                "task": {"type": "string", "description": "Complete, self-contained instructions. The worker sees nothing else."}
            }, "required": ["task"]}
        }},
        {"type": "function", "function": {
            "name": MESSAGE_WORKER,
            "description": "Send a follow-up instruction to an existing worker. It keeps its history; a busy worker reads it after its current instruction.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "text": {"type": "string"}
            }, "required": ["id", "text"]}
        }},
        {"type": "function", "function": {
            "name": LIST_WORKERS,
            "description": "List workers with their state (working, idle, failed), directory and initial task.",
            "parameters": {"type": "object", "properties": {}}
        }},
        {"type": "function", "function": {
            "name": LIST_REPOS,
            "description": "List the git repositories that already exist under the configured roots, so a worker can start in one. Never ask the user for a path.",
            "parameters": {"type": "object", "properties": {}}
        }}
    ])
}

pub(crate) fn worker_tool_definitions() -> Value {
    let mut defs = tools::definitions();
    if let Some(functions) = defs.as_array_mut() {
        functions.push(json!({"type": "function", "function": {
            "name": REPLY,
            "description": "Send a short progress note to Overseer without ending your turn.",
            "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
        }}));
    }
    defs
}

pub(crate) fn overseer_prompt(roots: &[PathBuf]) -> String {
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
    format!(
        "You are Overseer: you coordinate a fleet of worker agents and never do the work yourself.\n{}\n\
Roots you know about:\n{}\n\
Repositories that exist right now:\n{}\n\
Delegate every request to a worker, one worker per independent task, and give each worker complete instructions: it sees nothing else.\n\
Never ask the user for a path, a command output or any other fact a worker could find.\n\
Reuse an idle worker for follow-ups in the same directory instead of starting another. Put a task in the repository listed above when one matches; start a repository hunt only when none does.\n\
Messages that start with [worker N ...] come from workers, not the user: \"says\" is a progress note or question, \"finished\" carries the worker's final report, \"failed\" an error. \
Answer a worker question yourself when you can; otherwise ask the user. Report results to the user concisely.",
        tool_section(&overseer_tool_definitions()),
        listed.join("\n"),
        known
    )
}

pub(crate) fn worker_prompt(dir: &Path) -> String {
    format!(
        "You are a worker agent working in the directory {}; relative paths resolve there. Overseer, a coordinator, sends you instructions.\n{}\n\
Do the work yourself and keep Overseer informed as you go. When you stop calling tools, your final answer is delivered to Overseer, so make it a complete report. \
Searching: `rg` (or `git grep`) scoped to the project, never a bare recursive grep over a code root — vendored trees (`.venv`, `node_modules`, `target`) make that take minutes. \
If you need a decision to continue, end your turn with the question as your final answer.",
        dir.display(),
        tool_section(&worker_tool_definitions())
    )
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
            let description = function.get("description").and_then(Value::as_str).unwrap_or_default();
            Some(format!("- {name}: {description}"))
        })
        .collect();
    format!("Your tools (same schemas as the `tools` array of every request):\n{}", tools.join("\n"))
}
