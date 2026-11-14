use crate::agent;
use crate::config;
use crate::llm::{Llm, Message, ToolCall};
use crate::tools;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// Arguments of each overseer tool. `deny_unknown_fields` turns a wrong key into an error naming the right ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    path: Option<String>,
    task: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    id: usize,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum WorkerEvent {
    Reply,
    Finished,
    Failed,
}

impl WorkerEvent {
    pub(crate) fn verb(self) -> &'static str {
        match self {
            WorkerEvent::Reply => "says",
            WorkerEvent::Finished => "finished",
            WorkerEvent::Failed => "failed",
        }
    }
}

/// Everything that wakes the overseer.
#[derive(Debug, PartialEq)]
pub(crate) enum Event {
    User(String),
    /// User typed `exit`: quit now.
    Exit,
    /// Stdin closed: quit once no worker is busy.
    Eof,
    Worker {
        id: usize,
        kind: WorkerEvent,
        text: String,
    },
}

struct Worker {
    id: usize,
    dir: PathBuf,
    task: String,
    pending: usize,
    failed: bool,
    inbox: Sender<String>,
}

pub(crate) struct Fleet {
    llm: Arc<dyn Llm>,
    roots: Vec<PathBuf>,
    log_dir: PathBuf,
    sender: Sender<Event>,
    workers: Vec<Worker>,
}

impl Fleet {
    pub(crate) fn new(llm: Arc<dyn Llm>, roots: Vec<PathBuf>, log_dir: PathBuf, sender: Sender<Event>) -> Self {
        Self { llm, roots, log_dir, sender, workers: Vec::new() }
    }

    pub(crate) fn busy(&self) -> bool {
        self.workers.iter().any(|w| w.pending > 0)
    }

    /// Updates worker status from one of its events. Call before handing the event to the overseer.
    pub(crate) fn record(&mut self, id: usize, kind: WorkerEvent) {
        let Some(worker) = id.checked_sub(1).and_then(|i| self.workers.get_mut(i)) else { return };
        if kind != WorkerEvent::Reply {
            worker.pending = worker.pending.saturating_sub(1);
            worker.failed = kind == WorkerEvent::Failed;
        }
    }

    /// Runs one overseer tool call; the returned string goes back to the overseer model.
    pub(crate) fn execute(&mut self, call: &ToolCall) -> String {
        let arguments = call.function.arguments.as_str();
        let result = match call.function.name.as_str() {
            "spawn_worker" => parse::<SpawnArgs>(arguments).and_then(|args| {
                let path = args.path.as_deref().unwrap_or(DEFAULT_WORKDIR);
                self.spawn(path, &args.task).map(|id| format!("worker {id} started"))
            }),
            "message_worker" => parse::<MessageArgs>(arguments).and_then(|args| self.message(args.id, &args.text)),
            "list_workers" => parse::<NoArgs>(arguments).map(|_| self.list()),
            "list_repos" => parse::<NoArgs>(arguments).map(|_| self.list_repos()),
            other => Err(format!("unknown tool {other}")),
        };
        result.unwrap_or_else(|e| format!("error: {e}"))
    }

    fn spawn(&mut self, path: &str, task: &str) -> Result<usize, String> {
        let dir = config::resolve_dir(path)?;
        let id = self.workers.len() + 1;
        let log_path = self.log_dir.join(format!("worker-{id}.log"));
        let log = File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let (inbox, instructions) = mpsc::channel();
        inbox.send(task.to_string()).expect("the receiver lives in this scope");
        let (llm, sender, worker_dir) = (Arc::clone(&self.llm), self.sender.clone(), dir.clone());
        thread::spawn(move || {
            let mut log = log;
            run_worker(id, &worker_dir, llm.as_ref(), instructions, &sender, &mut log);
        });
        self.workers.push(Worker { id, dir, task: task.to_string(), pending: 1, failed: false, inbox });
        Ok(id)
    }

    fn message(&mut self, id: usize, text: &str) -> Result<String, String> {
        let worker =
            id.checked_sub(1).and_then(|i| self.workers.get_mut(i)).ok_or_else(|| format!("no worker {id}"))?;
        worker.inbox.send(text.to_string()).map_err(|_| format!("worker {id} is gone"))?;
        worker.pending += 1;
        Ok(if worker.pending > 1 {
            format!("queued for worker {id}; it will read it after its current instruction")
        } else {
            format!("sent to worker {id}")
        })
    }

    fn list(&self) -> String {
        if self.workers.is_empty() {
            return "no workers".into();
        }
        let lines: Vec<String> = self
            .workers
            .iter()
            .map(|w| {
                let state = if w.pending > 0 {
                    "working"
                } else if w.failed {
                    "failed"
                } else {
                    "idle"
                };
                let task: String = w.task.lines().next().unwrap_or("").chars().take(80).collect();
                format!("worker {} [{state}] {} | {task}", w.id, w.dir.display())
            })
            .collect();
        lines.join("\n")
    }

    fn list_repos(&self) -> String {
        let mut found = Vec::new();
        for root in &self.roots {
            collect_repos(root, REPO_DEPTH, &mut found);
        }
        if found.is_empty() {
            let roots: Vec<String> = self.roots.iter().map(|root| root.display().to_string()).collect();
            return format!("no git repositories found under {}", roots.join(", "));
        }
        found.sort();
        let total = found.len();
        found.truncate(MAX_REPOS);
        let mut lines: Vec<String> = found.iter().map(|path| path.display().to_string()).collect();
        if total > MAX_REPOS {
            lines.push(format!("[{total} repositories found; the first {MAX_REPOS} are listed]"));
        }
        lines.join("\n")
    }
}

/// Parses tool arguments, turning a serde failure into the text the model reads.
fn parse<'a, T: Deserialize<'a>>(arguments: &'a str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

/// Where a worker starts when `spawn_worker` names no directory.
const DEFAULT_WORKDIR: &str = "/tmp";

/// Levels below a root searched for repositories; a repository is never descended into.
const REPO_DEPTH: usize = 3;

/// Longest `list_repos` answer, before it says how many were cut.
const MAX_REPOS: usize = 300;

fn collect_repos(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_REPOS {
        return;
    }
    if dir.join(".git").exists() {
        out.push(dir.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| !path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')))
        .collect();
    children.sort();
    for child in children {
        collect_repos(&child, depth - 1, out);
    }
}

pub(crate) fn overseer_tool_definitions() -> Value {
    json!([
        {"type": "function", "function": {
            "name": "spawn_worker",
            "description": "Start a new worker agent in a directory. Returns the worker id at once; the worker's final answer arrives later as a [worker N finished] message.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Directory the worker works in; relative paths resolve there. Defaults to /tmp."},
                "task": {"type": "string", "description": "Complete, self-contained instructions. The worker sees nothing else."}
            }, "required": ["task"]}
        }},
        {"type": "function", "function": {
            "name": "message_worker",
            "description": "Send a follow-up instruction to an existing worker. It keeps its history; a busy worker reads it after its current instruction.",
            "parameters": {"type": "object", "properties": {
                "id": {"type": "integer"},
                "text": {"type": "string"}
            }, "required": ["id", "text"]}
        }},
        {"type": "function", "function": {
            "name": "list_workers",
            "description": "List workers with their state (working, idle, failed), directory and task.",
            "parameters": {"type": "object", "properties": {}}
        }},
        {"type": "function", "function": {
            "name": "list_repos",
            "description": "List the git repositories that already exist under the configured roots, so a worker can start in one. Never ask the user for a path.",
            "parameters": {"type": "object", "properties": {}}
        }}
    ])
}

pub(crate) fn overseer_prompt(roots: &[PathBuf]) -> String {
    let listed: Vec<String> = roots.iter().map(|root| format!("- {}", root.display())).collect();
    let mut repos = Vec::new();
    for root in roots {
        collect_repos(root, REPO_DEPTH, &mut repos);
    }
    repos.sort();
    let known = if repos.is_empty() {
        "none found under those roots".to_string()
    } else {
        repos.iter().map(|repo| format!("- {}", repo.display())).collect::<Vec<_>>().join("\n")
    };
    format!(
        "You are Overseer: you coordinate a fleet of worker agents and never do the work yourself.\n{}\n\
Roots you know about:\n{}\n\
Repositories that exist right now:\n{}\n\
Delegate every request to a worker, one worker per independent task, and give each worker complete instructions: it sees nothing else.\n\
Never ask the user for a path, a command output or any other fact a worker could find.\n\
Reuse an idle worker for follow-ups in the same directory instead of starting another.\n\
Messages that start with [worker N ...] come from workers, not the user: \"says\" is a progress note or question, \"finished\" carries the worker's final report, \"failed\" an error. \
Answer a worker question yourself when you can; otherwise ask the user. Report results to the user concisely.",
        tool_section(&overseer_tool_definitions()),
        listed.join("\n"),
        known
    )
}

fn worker_prompt(dir: &Path) -> String {
    format!(
        "You are a worker agent working in the directory {}; relative paths resolve there. Overseer, a coordinator, sends you instructions.\n{}\n\
Do the work yourself and keep Overseer informed as you go. When you stop calling tools, your final answer is delivered to Overseer, so make it a complete report. \
If you need a decision to continue, end your turn with the question as your final answer.",
        dir.display(),
        tool_section(&worker_tool_definitions())
    )
}

/// Renders tool definitions into prompt lines, so a tool is described once, in `*_tool_definitions`.
fn tool_section(defs: &Value) -> String {
    let tools: Vec<String> = defs
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?;
            let description = function.get("description").and_then(Value::as_str).unwrap_or_default();
            let arguments = function.get("parameters").map_or_else(String::new, ToString::to_string);
            Some(format!("- {name}: {description}\n  arguments: {arguments}"))
        })
        .collect();
    format!("Your tools (same schemas as the `tools` array of every request):\n{}", tools.join("\n"))
}

fn worker_tool_definitions() -> Value {
    let mut defs = tools::definitions();
    defs.as_array_mut().expect("tools::definitions returns an array").push(json!({"type": "function", "function": {
        "name": "reply",
        "description": "Send a short progress note to Overseer without ending your turn.",
        "parameters": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
    }}));
    defs
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    text: String,
}

fn reply_text(arguments: &str) -> Result<String, String> {
    parse::<ReplyArgs>(arguments).map(|args| args.text)
}

fn run_worker(
    id: usize,
    dir: &Path,
    llm: &dyn Llm,
    instructions: Receiver<String>,
    sender: &Sender<Event>,
    log: &mut File,
) {
    let mut history = vec![Message::system(worker_prompt(dir))];
    let tool_defs = worker_tool_definitions();
    // Log writes are best effort: a full disk must not kill the worker.
    for input in instructions {
        let _ = writeln!(log, "> {input}");
        let result = {
            let mut execute = |call: &ToolCall| {
                let _ = writeln!(log, "· {} {}", call.function.name, call.function.arguments);
                let output = if call.function.name == "reply" {
                    match reply_text(&call.function.arguments) {
                        Ok(text) => {
                            let _ = sender.send(Event::Worker { id, kind: WorkerEvent::Reply, text });
                            "sent to overseer".to_string()
                        }
                        Err(e) => format!("error: {e}"),
                    }
                } else {
                    tools::execute(dir, &call.function.name, &call.function.arguments)
                };
                let _ = writeln!(log, "{output}");
                output
            };
            agent::run_turn(llm, &mut history, &input, &tool_defs, &mut execute)
        };
        let (kind, text) = match result {
            Ok(answer) => (WorkerEvent::Finished, answer),
            Err(e) => (WorkerEvent::Failed, format!("{e:#}")),
        };
        let _ = writeln!(log, "= {kind:?}: {text}");
        let _ = sender.send(Event::Worker { id, kind, text });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::testing::{Scripted, call, text, tool_call};
    use std::fs;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    struct Harness {
        fleet: Fleet,
        queue: Receiver<Event>,
        repo: PathBuf,
        logs: tempfile::TempDir,
        root: tempfile::TempDir,
    }

    fn harness(replies: Vec<Message>) -> Harness {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let (sender, queue) = mpsc::channel();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        let fleet = Fleet::new(Arc::new(Scripted::new(replies)), roots, logs.path().to_path_buf(), sender);
        Harness { fleet, queue, repo, logs, root }
    }

    fn spawn(fleet: &mut Fleet, dir: &Path) -> String {
        fleet.execute(&tool_call("s", "spawn_worker", &json!({"path": dir, "task": "do it"})))
    }

    fn list(fleet: &mut Fleet) -> String {
        fleet.execute(&tool_call("l", "list_workers", &json!({})))
    }

    fn repos(fleet: &mut Fleet) -> String {
        fleet.execute(&tool_call("r", "list_repos", &json!({})))
    }

    #[test]
    fn list_repos_reports_git_directories_and_nothing_else() {
        let mut h = harness(vec![]);
        assert_eq!(
            repos(&mut h.fleet),
            format!("no git repositories found under {}", fs::canonicalize(h.root.path()).unwrap().display())
        );
        fs::create_dir(h.repo.join(".git")).unwrap();
        fs::create_dir(h.root.path().join("plain")).unwrap();
        fs::create_dir_all(h.repo.join("nested/.git")).unwrap();
        assert_eq!(repos(&mut h.fleet), fs::canonicalize(&h.repo).unwrap().display().to_string());
    }

    #[test]
    fn overseer_prompt_lists_existing_repositories() {
        let h = harness(vec![]);
        fs::create_dir(h.repo.join(".git")).unwrap();
        let roots = vec![fs::canonicalize(h.root.path()).unwrap()];
        let prompt = overseer_prompt(&roots);
        assert!(prompt.contains(&fs::canonicalize(&h.repo).unwrap().display().to_string()), "{prompt}");
    }

    #[test]
    fn prompts_render_every_tool_from_its_definitions() {
        let prompt = overseer_prompt(&[]);
        for tool in overseer_tool_definitions().as_array().unwrap() {
            let name = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            assert!(prompt.contains(name), "{name} missing from the overseer prompt");
        }
        let prompt = worker_prompt(Path::new("/tmp"));
        for tool in worker_tool_definitions().as_array().unwrap() {
            let name = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            assert!(prompt.contains(name), "{name} missing from the worker prompt");
        }
    }
    #[test]
    fn worker_reply_and_final_answer_reach_overseer() {
        let mut h = harness(vec![call("c1", "reply", &json!({"text": "halfway"})), text("done")]);
        assert_eq!(spawn(&mut h.fleet, &h.repo), "worker 1 started");
        assert!(h.fleet.busy());
        let reply = Event::Worker { id: 1, kind: WorkerEvent::Reply, text: "halfway".into() };
        assert_eq!(h.queue.recv_timeout(WAIT).unwrap(), reply);
        let finished = Event::Worker { id: 1, kind: WorkerEvent::Finished, text: "done".into() };
        assert_eq!(h.queue.recv_timeout(WAIT).unwrap(), finished);
        h.fleet.record(1, WorkerEvent::Finished);
        assert!(!h.fleet.busy());
        assert!(list(&mut h.fleet).starts_with("worker 1 [idle]"));
        let log = fs::read_to_string(h.logs.path().join("worker-1.log")).unwrap();
        assert!(log.contains("> do it") && log.contains("· reply") && log.contains("= Finished: done"), "{log}");
    }

    #[test]
    fn message_reaches_existing_worker_history() {
        let mut h = harness(vec![text("first"), text("second")]);
        spawn(&mut h.fleet, &h.repo);
        h.queue.recv_timeout(WAIT).unwrap();
        h.fleet.record(1, WorkerEvent::Finished);
        let sent = h.fleet.execute(&tool_call("m", "message_worker", &json!({"id": 1, "text": "more"})));
        assert_eq!(sent, "sent to worker 1");
        let finished = Event::Worker { id: 1, kind: WorkerEvent::Finished, text: "second".into() };
        assert_eq!(h.queue.recv_timeout(WAIT).unwrap(), finished);
    }

    #[test]
    fn failed_turn_is_reported() {
        let mut h = harness(vec![]);
        spawn(&mut h.fleet, &h.repo);
        let failed = Event::Worker { id: 1, kind: WorkerEvent::Failed, text: "script exhausted".into() };
        assert_eq!(h.queue.recv_timeout(WAIT).unwrap(), failed);
        h.fleet.record(1, WorkerEvent::Failed);
        assert!(list(&mut h.fleet).starts_with("worker 1 [failed]"));
    }

    #[test]
    fn spawn_takes_any_directory_and_defaults_to_tmp() {
        let mut h = harness(vec![text("a"), text("b")]);
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(spawn(&mut h.fleet, elsewhere.path()), "worker 1 started");
        let default = h.fleet.execute(&tool_call("d", "spawn_worker", &json!({"task": "do it"})));
        assert_eq!(default, "worker 2 started");
        let listed = list(&mut h.fleet);
        assert!(listed.contains("/tmp"), "{listed}");
        assert_eq!(
            h.fleet.execute(&tool_call("m", "message_worker", &json!({"id": 7, "text": "x"}))),
            "error: no worker 7"
        );
        // The overseer has no work tools.
        assert_eq!(h.fleet.execute(&tool_call("x", "bash", &json!({}))), "error: unknown tool bash");
        assert!(
            h.fleet
                .execute(&tool_call("r", "spawn_worker", &json!({"repo": "/tmp", "task": "x"})))
                .starts_with("error: invalid arguments: unknown field `repo`, expected `path` or `task`")
        );
        assert_eq!(
            h.fleet.execute(&tool_call("s", "spawn_worker", &json!({"path": "/nonexistent/x", "task": "x"}))),
            "error: /nonexistent/x: No such file or directory (os error 2)"
        );
    }
}
