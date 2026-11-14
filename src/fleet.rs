use crate::agent;
use crate::config;
use crate::llm::{Llm, Message, ToolCall};
use crate::tools;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

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
    repo: PathBuf,
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
        let args: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(e) => return format!("error: invalid arguments: {e}"),
        };
        let result = match call.function.name.as_str() {
            "spawn_worker" => tools::str_arg(&args, "repo")
                .and_then(|repo| tools::str_arg(&args, "task").and_then(|task| self.spawn(repo, task)))
                .map(|id| format!("worker {id} started")),
            "message_worker" => args
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|id| usize::try_from(id).ok())
                .ok_or_else(|| "missing integer argument 'id'".to_string())
                .and_then(|id| tools::str_arg(&args, "text").and_then(|text| self.message(id, text))),
            "list_workers" => Ok(self.list()),
            other => Err(format!("unknown tool {other}")),
        };
        result.unwrap_or_else(|e| format!("error: {e}"))
    }

    fn spawn(&mut self, repo: &str, task: &str) -> Result<usize, String> {
        let repo = config::resolve_repo(&self.roots, repo)?;
        let id = self.workers.len() + 1;
        let log_path = self.log_dir.join(format!("worker-{id}.log"));
        let log = File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let (inbox, instructions) = mpsc::channel();
        inbox.send(task.to_string()).expect("the receiver lives in this scope");
        let (llm, sender, worker_repo) = (Arc::clone(&self.llm), self.sender.clone(), repo.clone());
        thread::spawn(move || {
            let mut log = log;
            run_worker(id, &worker_repo, llm.as_ref(), instructions, &sender, &mut log);
        });
        self.workers.push(Worker { id, repo, task: task.to_string(), pending: 1, failed: false, inbox });
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
                format!("worker {} [{state}] {} | {task}", w.id, w.repo.display())
            })
            .collect();
        lines.join("\n")
    }
}

pub(crate) fn overseer_tool_definitions() -> Value {
    json!([
        {"type": "function", "function": {
            "name": "spawn_worker",
            "description": "Start a new worker agent in a repository. Returns the worker id at once; the worker's final answer arrives later as a [worker N finished] message.",
            "parameters": {"type": "object", "properties": {
                "repo": {"type": "string", "description": "Absolute path of an existing repository directory inside one of the roots."},
                "task": {"type": "string", "description": "Complete, self-contained instructions. The worker sees nothing else."}
            }, "required": ["repo", "task"]}
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
            "description": "List workers with their state (working, idle, failed), repository and task.",
            "parameters": {"type": "object", "properties": {}}
        }}
    ])
}

pub(crate) fn overseer_prompt(roots: &[PathBuf]) -> String {
    let roots: Vec<String> = roots.iter().map(|r| format!("- {}", r.display())).collect();
    format!(
        "You are Overseer. You coordinate a fleet of worker agents and never do the work yourself: you cannot read files or run commands. \
Delegate every request with spawn_worker, one worker per independent task, each in an existing repository under one of these roots:\n{}\n\
Workers never clone repositories. Give each worker complete instructions. Reuse an idle worker with message_worker for follow-ups in the same repository instead of spawning a new one. Check the fleet with list_workers.\n\
Messages that start with [worker N ...] come from workers, not the user: \"says\" is a progress note or question, \"finished\" carries the worker's final report, \"failed\" an error. \
Answer a worker question yourself with message_worker when you can; otherwise ask the user. Report results to the user concisely.",
        roots.join("\n")
    )
}

fn worker_prompt(repo: &Path) -> String {
    format!(
        "You are a worker agent in the repository {}. Relative paths resolve there. Overseer, a coordinator, sends you instructions. \
Do the work with bash, read_file and write_file. Never clone repositories. \
Use reply to send Overseer a progress note mid-task. When you stop calling tools, your final answer is delivered to Overseer automatically, so make it a complete report of what you did. \
If you need a decision to continue, end your turn with the question as your final answer.",
        repo.display()
    )
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

fn reply_text(arguments: &str) -> Result<String, String> {
    let args: Value = serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))?;
    tools::str_arg(&args, "text").map(str::to_string)
}

fn run_worker(
    id: usize,
    repo: &Path,
    llm: &dyn Llm,
    instructions: Receiver<String>,
    sender: &Sender<Event>,
    log: &mut File,
) {
    let mut history = vec![Message::system(worker_prompt(repo))];
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
                    tools::execute(repo, &call.function.name, &call.function.arguments)
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
        _root: tempfile::TempDir,
    }

    fn harness(replies: Vec<Message>) -> Harness {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let (sender, queue) = mpsc::channel();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        let fleet = Fleet::new(Arc::new(Scripted::new(replies)), roots, logs.path().to_path_buf(), sender);
        Harness { fleet, queue, repo, logs, _root: root }
    }

    fn spawn(fleet: &mut Fleet, repo: &Path) -> String {
        fleet.execute(&tool_call("s", "spawn_worker", &json!({"repo": repo, "task": "do it"})))
    }

    fn list(fleet: &mut Fleet) -> String {
        fleet.execute(&tool_call("l", "list_workers", &json!({})))
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
    fn rejects_bad_targets() {
        let mut h = harness(vec![]);
        let outside = tempfile::tempdir().unwrap();
        assert!(spawn(&mut h.fleet, outside.path()).contains("is outside the configured roots"));
        assert_eq!(
            h.fleet.execute(&tool_call("m", "message_worker", &json!({"id": 7, "text": "x"}))),
            "error: no worker 7"
        );
        // The overseer has no work tools.
        assert_eq!(h.fleet.execute(&tool_call("x", "bash", &json!({}))), "error: unknown tool bash");
        assert_eq!(list(&mut h.fleet), "no workers");
    }
}
