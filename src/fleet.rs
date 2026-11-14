use crate::agent::Turn;
use crate::config;
use crate::llm::{Llm, Message, StreamLlm, ToolCall};
use crate::prompt;
use crate::repos;
use crate::tools;
use serde::Deserialize;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Longest one-line summary of a tool call.
const PROGRESS_WIDTH: usize = 100;

/// Longest task preview in `list_workers`.
const TASK_PREVIEW_WIDTH: usize = 80;

/// Message `/resume` sends: the worker keeps its history, so its task is already in it.
const CONTINUE: &str = "continue";

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    text: String,
}

/// One worker tool call, as the user sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Progress {
    /// A tool call started; `summary` is one line of its arguments.
    Started { tool: String, summary: String },
    /// The same call returned after `secs`.
    Finished { tool: String, secs: f64 },
}

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
    /// Input closed: quit once no worker is busy.
    Eof,
    /// User typed `/resume <id>`: the argument, handled without a model call.
    Resume(String),
    Worker {
        id: usize,
        kind: WorkerEvent,
        text: String,
    },
    /// Worker tool activity: shown to the user, never fed to the overseer model.
    Progress {
        id: usize,
        progress: Progress,
    },
    /// Worker model output as it streams: shown to the user, never fed to the overseer model.
    Delta {
        id: usize,
        text: String,
    },
}

/// One worker as the panels show it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkerView {
    pub(crate) id: usize,
    pub(crate) dir: PathBuf,
    /// The instruction the worker was started with.
    pub(crate) task: String,
    /// Instructions sent but not yet finished.
    pub(crate) pending: usize,
    pub(crate) failed: bool,
    pub(crate) finished: bool,
    /// How long the current instruction has been running.
    pub(crate) busy_for: Option<Duration>,
}

/// Sends a progress line the user sees, without waking the overseer model.
fn progress(sender: &Sender<Event>, id: usize, progress: Progress) {
    let _ = sender.send(Event::Progress { id, progress });
}

/// One line naming what a tool call acts on: the transcript header and the worker panel both show it.
pub(crate) fn describe(name: &str, arguments: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let text = |key: &str| args.get(key).and_then(serde_json::Value::as_str).unwrap_or_default();
    let first = |key: &str| text(key).lines().next().unwrap_or_default().to_string();
    let line = match name {
        prompt::SPAWN_WORKER => {
            let path = text("path");
            let dir = if path.is_empty() { prompt::DEFAULT_WORKING_DIR } else { path };
            format!("{dir} · {}", first("task"))
        }
        prompt::MESSAGE_WORKER => {
            let id = args.get("id").and_then(serde_json::Value::as_u64).unwrap_or_default();
            format!("#{id} · {}", first("text"))
        }
        prompt::REPLY => first("text"),
        tools::BASH => first("command"),
        tools::READ_FILE | tools::WRITE_FILE => text("path").to_string(),
        _ => String::new(),
    };
    let mut short: String = line.chars().take(PROGRESS_WIDTH).collect();
    if line.chars().count() > PROGRESS_WIDTH {
        short.push('…');
    }
    short
}

struct Worker {
    id: usize,
    working_dir: PathBuf,
    /// The instruction the worker was started with; follow-up messages do not replace it.
    initial_task: String,
    /// Instructions sent but not yet finished. A `Reply` does not count.
    pending: usize,
    failed: bool,
    inbox: Sender<String>,
    /// When the current instruction started; `None` once the worker is idle.
    busy_since: Option<Instant>,
    /// The worker thread. Its exit is the authoritative liveness signal.
    handle: JoinHandle<()>,
}

impl Worker {
    fn finished(&self) -> bool {
        self.handle.is_finished()
    }
}

pub(crate) struct Fleet {
    llm: Arc<dyn Llm>,
    roots: Vec<PathBuf>,
    home: PathBuf,
    log_dir: PathBuf,
    sender: Sender<Event>,
    workers: Vec<Worker>,
}

impl Fleet {
    pub(crate) fn new(
        llm: Arc<dyn Llm>,
        roots: Vec<PathBuf>,
        home: PathBuf,
        log_dir: PathBuf,
        sender: Sender<Event>,
    ) -> Self {
        Self { llm, roots, home, log_dir, sender, workers: Vec::new() }
    }

    /// The roster as the frontends draw it.
    pub(crate) fn snapshot(&self) -> Vec<WorkerView> {
        self.workers
            .iter()
            .map(|w| WorkerView {
                id: w.id,
                dir: w.working_dir.clone(),
                task: w.initial_task.clone(),
                pending: w.pending,
                failed: w.failed,
                finished: w.finished(),
                busy_for: w.busy_since.map(|since| since.elapsed()),
            })
            .collect()
    }

    pub(crate) fn busy(&self) -> bool {
        self.workers.iter().any(|w| w.pending > 0 && !w.finished())
    }

    /// Updates worker status from one of its events. Call before handing the event to the overseer.
    pub(crate) fn record(&mut self, id: usize, kind: WorkerEvent) {
        let Some(worker) = self.worker_mut(id) else { return };
        if kind != WorkerEvent::Reply {
            worker.pending = worker.pending.saturating_sub(1);
            if worker.pending == 0 {
                worker.busy_since = None;
            }
            worker.failed = kind == WorkerEvent::Failed;
        }
    }

    /// Re-tasks a worker with a continue message, clearing a previous failure. Returns the line to print.
    pub(crate) fn resume(&mut self, arg: &str) -> String {
        let result = resume_id(arg).and_then(|id| self.message_worker(id, CONTINUE, true).map(|_| id));
        match result {
            Ok(id) => format!("worker {id} resumed"),
            Err(e) => format!("error: {e}; worker ids: {}", self.ids()),
        }
    }

    /// Ids that exist, so an error can name the ones `/resume` accepts.
    fn ids(&self) -> String {
        if self.workers.is_empty() {
            return "none".into();
        }
        self.workers.iter().map(|w| w.id.to_string()).collect::<Vec<String>>().join(", ")
    }

    /// Runs one overseer tool call; the returned string goes back to the overseer model.
    ///
    /// `spawn_worker` has durable effects: it creates a log file and starts a worker thread.
    pub(crate) fn execute(&mut self, call: &ToolCall) -> String {
        let arguments = call.function.arguments.as_str();
        let result = match call.function.name.as_str() {
            prompt::SPAWN_WORKER => tools::parse_args::<SpawnArgs>(arguments).and_then(|args| {
                let path = args.path.as_deref().unwrap_or(prompt::DEFAULT_WORKING_DIR);
                self.start_worker(path, &args.task).map(|id| format!("worker {id} started"))
            }),
            prompt::MESSAGE_WORKER => tools::parse_args::<MessageArgs>(arguments)
                .and_then(|args| self.message_worker(args.id, &args.text, false)),
            prompt::LIST_WORKERS => tools::parse_args::<NoArgs>(arguments).map(|_| self.list_workers()),
            prompt::LIST_REPOS => tools::parse_args::<NoArgs>(arguments).map(|_| self.list_repos()),
            other => Err(format!("unknown tool {other}")),
        };
        tools::outcome(result)
    }

    /// The one place a 1-based worker id becomes a 0-based index.
    fn worker_mut(&mut self, id: usize) -> Option<&mut Worker> {
        id.checked_sub(1).and_then(|i| self.workers.get_mut(i))
    }

    fn start_worker(&mut self, path: &str, task: &str) -> Result<usize, String> {
        let working_dir = config::resolve_dir(path, &self.home)?;
        let id = self.workers.len() + 1;
        let log_path = self.log_dir.join(format!("worker-{id}.log"));
        let log = File::create(&log_path).map_err(|e| config::io_error(log_path.display(), &e))?;
        let (inbox, instructions) = mpsc::channel();
        inbox.send(task.to_string()).expect("the receiver lives in this scope");
        let (llm, sender) = (Arc::clone(&self.llm), self.sender.clone());
        let stream_sender = sender.clone();
        let streamed = Arc::new(StreamLlm::new(
            llm,
            Arc::new(move |delta: &str| {
                let _ = stream_sender.send(Event::Delta { id, text: delta.to_string() });
            }),
        ));
        let worker_dir = working_dir.clone();
        let handle = thread::spawn(move || {
            let mut log = log;
            let mut run = WorkerRun { id, working_dir: &worker_dir, sender: &sender, log: &mut log };
            run.run(streamed.as_ref(), instructions);
        });
        self.workers.push(Worker {
            id,
            working_dir,
            initial_task: task.to_string(),
            pending: 1,
            failed: false,
            busy_since: Some(Instant::now()),
            inbox,
            handle,
        });
        Ok(id)
    }

    fn message_worker(&mut self, id: usize, text: &str, clear_failure: bool) -> Result<String, String> {
        let worker = self.worker_mut(id).ok_or_else(|| format!("no worker {id}"))?;
        if worker.finished() {
            return Err(format!("worker {id} is gone"));
        }
        worker.inbox.send(text.to_string()).map_err(|_| format!("worker {id} is gone"))?;
        worker.pending += 1;
        worker.busy_since = Some(Instant::now());
        if clear_failure {
            worker.failed = false;
        }
        Ok(if worker.pending > 1 {
            format!("queued for worker {id}; it will read it after its current instruction")
        } else {
            format!("sent to worker {id}")
        })
    }

    fn list_workers(&self) -> String {
        if self.workers.is_empty() {
            return "no workers".into();
        }
        let lines: Vec<String> = self
            .workers
            .iter()
            .map(|w| {
                let state = if w.pending > 0 {
                    if w.finished() { "failed" } else { "working" }
                } else if w.failed {
                    "failed"
                } else {
                    "idle"
                };
                let task: String =
                    w.initial_task.lines().next().unwrap_or("").chars().take(TASK_PREVIEW_WIDTH).collect();
                format!("worker {} [{state}] {} | {task}", w.id, w.working_dir.display())
            })
            .collect();
        lines.join("\n")
    }

    fn list_repos(&self) -> String {
        let found = repos::find(&self.roots);
        if found.is_empty() {
            let roots: Vec<String> = self.roots.iter().map(|root| root.display().to_string()).collect();
            return format!("no git repositories found under {}", roots.join(", "));
        }
        let total = found.len();
        let mut lines: Vec<String> =
            found.iter().take(prompt::MAX_REPOS).map(|path| path.display().to_string()).collect();
        if total > prompt::MAX_REPOS {
            lines.push(format!("[{total} repositories found; the first {} are listed]", prompt::MAX_REPOS));
        }
        lines.join("\n")
    }
}

/// Reads the one worker id `/resume` takes, or why the argument is unusable.
fn resume_id(arg: &str) -> Result<usize, String> {
    let mut parts = arg.split_whitespace();
    match (parts.next(), parts.next()) {
        (None, _) => Err("usage: /resume <id>".into()),
        (Some(id), None) => id.parse::<usize>().map_err(|_| format!("not a worker id: {id}")),
        (Some(_), Some(_)) => Err(format!("expected one worker id, got: {}", arg.trim())),
    }
}

/// One worker thread: its identity, its working directory, where progress goes, and its log.
struct WorkerRun<'a> {
    id: usize,
    working_dir: &'a Path,
    sender: &'a Sender<Event>,
    log: &'a mut File,
}

impl WorkerRun<'_> {
    /// Runs every instruction the worker receives until its inbox closes.
    fn run(&mut self, llm: &dyn Llm, instructions: Receiver<String>) {
        let mut history = vec![Message::system(prompt::worker_prompt(self.working_dir))];
        let tool_defs = prompt::worker_tool_definitions();
        // Log writes are best effort: a full disk must not kill the worker.
        for input in instructions {
            let _ = writeln!(self.log, "> {input}");
            let result = {
                let mut turn = Turn { llm, tool_defs: &tool_defs, execute: &mut |call: &ToolCall| self.execute(call) };
                turn.run(&mut history, &input)
            };
            let (kind, text) = match result {
                Ok(answer) => (WorkerEvent::Finished, answer),
                Err(e) => (WorkerEvent::Failed, format!("{e:#}")),
            };
            let _ = writeln!(self.log, "= {kind:?}: {text}");
            let _ = self.sender.send(Event::Worker { id: self.id, kind, text });
        }
    }

    /// Runs one tool call, logging it and reporting its progress to the user.
    fn execute(&mut self, call: &ToolCall) -> String {
        let _ = writeln!(self.log, "· {} {}", call.function.name, call.function.arguments);
        let started = Instant::now();
        let tracked = call.function.name != prompt::REPLY;
        if tracked {
            let summary = describe(&call.function.name, &call.function.arguments);
            progress(self.sender, self.id, Progress::Started { tool: call.function.name.clone(), summary });
        }
        let output = if call.function.name == prompt::REPLY {
            match tools::parse_args::<ReplyArgs>(&call.function.arguments).map(|args| args.text) {
                Ok(text) => {
                    let _ = self.sender.send(Event::Worker { id: self.id, kind: WorkerEvent::Reply, text });
                    "sent to overseer".to_string()
                }
                Err(e) => format!("error: {e}"),
            }
        } else {
            tools::execute(self.working_dir, &call.function.name, &call.function.arguments)
        };
        let _ = writeln!(self.log, "{output}");
        if tracked {
            let secs = started.elapsed().as_secs_f64();
            progress(self.sender, self.id, Progress::Finished { tool: call.function.name.clone(), secs });
        }
        output
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::testing::{Scripted, call, text, tool_call};
    use serde_json::{Value, json};
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

    /// The next event that is not streamed model output: these tests assert worker outcomes.
    fn next_event(queue: &Receiver<Event>) -> Event {
        loop {
            match queue.recv_timeout(WAIT).unwrap() {
                Event::Delta { .. } => {}
                event => return event,
            }
        }
    }

    fn harness(replies: Vec<Message>) -> Harness {
        harness_with(Arc::new(Scripted::new(replies)))
    }

    fn harness_with(llm: Arc<dyn Llm>) -> Harness {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let (sender, queue) = mpsc::channel();
        let roots = vec![fs::canonicalize(root.path()).unwrap()];
        let fleet = Fleet::new(llm, roots, root.path().to_path_buf(), logs.path().to_path_buf(), sender);
        Harness { fleet, queue, repo, logs, root }
    }

    fn spawn(fleet: &mut Fleet, dir: &Path) -> String {
        fleet.execute(&tool_call("s", prompt::SPAWN_WORKER, &json!({"path": dir, "task": "do it"})))
    }

    fn list(fleet: &mut Fleet) -> String {
        fleet.execute(&tool_call("l", prompt::LIST_WORKERS, &json!({})))
    }

    fn repos(fleet: &mut Fleet) -> String {
        fleet.execute(&tool_call("r", prompt::LIST_REPOS, &json!({})))
    }

    #[test]
    fn every_overseer_tool_is_dispatched() {
        let mut h = harness(vec![]);
        for tool in prompt::overseer_tool_definitions().as_array().unwrap() {
            let name = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            let out = h.fleet.execute(&tool_call("t", name, &json!({})));
            assert!(!out.starts_with("error: unknown tool"), "{name} is defined but not dispatched");
        }
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
        let rendered = prompt::overseer_prompt(&roots);
        assert!(rendered.contains(&fs::canonicalize(&h.repo).unwrap().display().to_string()), "{rendered}");
    }

    #[test]
    fn prompts_render_every_tool_from_its_definitions() {
        let rendered = prompt::overseer_prompt(&[]);
        for tool in prompt::overseer_tool_definitions().as_array().unwrap() {
            let name = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            assert!(rendered.contains(name), "{name} missing from the overseer prompt");
        }
        let rendered = prompt::worker_prompt(Path::new("/tmp"));
        for tool in prompt::worker_tool_definitions().as_array().unwrap() {
            let name = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            assert!(rendered.contains(name), "{name} missing from the worker prompt");
        }
    }

    #[test]
    fn worker_reply_and_final_answer_reach_overseer() {
        let mut h = harness(vec![call("c1", prompt::REPLY, &json!({"text": "halfway"})), text("done")]);
        assert_eq!(spawn(&mut h.fleet, &h.repo), "worker 1 started");
        assert!(h.fleet.busy());
        let reply = Event::Worker { id: 1, kind: WorkerEvent::Reply, text: "halfway".into() };
        assert_eq!(next_event(&h.queue), reply);
        let finished = Event::Worker { id: 1, kind: WorkerEvent::Finished, text: "done".into() };
        assert_eq!(next_event(&h.queue), finished);
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
        next_event(&h.queue);
        h.fleet.record(1, WorkerEvent::Finished);
        let sent = h.fleet.execute(&tool_call("m", prompt::MESSAGE_WORKER, &json!({"id": 1, "text": "more"})));
        assert_eq!(sent, "sent to worker 1");
        let finished = Event::Worker { id: 1, kind: WorkerEvent::Finished, text: "second".into() };
        assert_eq!(next_event(&h.queue), finished);
    }

    #[test]
    fn failed_turn_is_reported() {
        let mut h = harness(vec![]);
        spawn(&mut h.fleet, &h.repo);
        let failed = Event::Worker { id: 1, kind: WorkerEvent::Failed, text: "script exhausted".into() };
        assert_eq!(next_event(&h.queue), failed);
        h.fleet.record(1, WorkerEvent::Failed);
        assert!(list(&mut h.fleet).starts_with("worker 1 [failed]"));
    }

    #[test]
    fn a_panicking_worker_does_not_keep_the_fleet_busy() {
        struct Panic;
        impl Llm for Panic {
            fn chat(&self, _: &[Message], _: &Value) -> anyhow::Result<Message> {
                panic!("boom")
            }
        }
        let mut h = harness_with(Arc::new(Panic));
        spawn(&mut h.fleet, &h.repo);
        let deadline = Instant::now() + WAIT;
        while h.fleet.busy() {
            assert!(Instant::now() < deadline, "a dead worker kept the fleet busy");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(list(&mut h.fleet).starts_with("worker 1 [failed]"));
    }

    #[test]
    fn spawn_takes_any_directory_and_defaults_to_tmp() {
        let mut h = harness(vec![text("a"), text("b")]);
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(spawn(&mut h.fleet, elsewhere.path()), "worker 1 started");
        let default = h.fleet.execute(&tool_call("d", prompt::SPAWN_WORKER, &json!({"task": "do it"})));
        assert_eq!(default, "worker 2 started");
        let listed = list(&mut h.fleet);
        assert!(listed.contains("/tmp"), "{listed}");
        assert_eq!(
            h.fleet.execute(&tool_call("m", prompt::MESSAGE_WORKER, &json!({"id": 7, "text": "x"}))),
            "error: no worker 7"
        );
        // The overseer has no work tools.
        assert_eq!(h.fleet.execute(&tool_call("x", tools::BASH, &json!({}))), "error: unknown tool bash");
        assert!(
            h.fleet
                .execute(&tool_call("r", prompt::SPAWN_WORKER, &json!({"repo": "/tmp", "task": "x"})))
                .starts_with("error: invalid arguments: unknown field `repo`, expected `path` or `task`")
        );
        assert_eq!(
            h.fleet.execute(&tool_call("s", prompt::SPAWN_WORKER, &json!({"path": "/nonexistent/x", "task": "x"}))),
            "error: /nonexistent/x: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn resume_retasks_an_idle_worker() {
        let mut h = harness(vec![text("first"), text("second")]);
        spawn(&mut h.fleet, &h.repo);
        next_event(&h.queue);
        h.fleet.record(1, WorkerEvent::Finished);
        assert_eq!(h.fleet.resume("1"), "worker 1 resumed");
        assert!(list(&mut h.fleet).starts_with("worker 1 [working]"));
        let finished = Event::Worker { id: 1, kind: WorkerEvent::Finished, text: "second".into() };
        assert_eq!(next_event(&h.queue), finished);
    }

    #[test]
    fn resume_clears_a_failed_worker() {
        let mut h = harness(vec![text("first"), text("second")]);
        spawn(&mut h.fleet, &h.repo);
        next_event(&h.queue);
        h.fleet.record(1, WorkerEvent::Failed);
        assert!(list(&mut h.fleet).starts_with("worker 1 [failed]"));
        assert_eq!(h.fleet.resume("1"), "worker 1 resumed");
        let finished = Event::Worker { id: 1, kind: WorkerEvent::Finished, text: "second".into() };
        assert_eq!(next_event(&h.queue), finished);
        h.fleet.record(1, WorkerEvent::Finished);
        assert!(list(&mut h.fleet).starts_with("worker 1 [idle]"));
    }

    #[test]
    fn resume_rejects_bad_arguments_without_messaging() {
        let mut h = harness(vec![text("first")]);
        assert_eq!(h.fleet.resume("1"), "error: no worker 1; worker ids: none");
        spawn(&mut h.fleet, &h.repo);
        next_event(&h.queue);
        h.fleet.record(1, WorkerEvent::Finished);
        assert_eq!(h.fleet.resume(""), "error: usage: /resume <id>; worker ids: 1");
        assert_eq!(h.fleet.resume("x"), "error: not a worker id: x; worker ids: 1");
        assert_eq!(h.fleet.resume("1 2"), "error: expected one worker id, got: 1 2; worker ids: 1");
        assert_eq!(h.fleet.resume("9"), "error: no worker 9; worker ids: 1");
        assert!(!h.fleet.busy());
        while h.queue.recv_timeout(Duration::from_millis(100)).is_ok() {}
        assert!(h.queue.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn resume_tolerates_surrounding_whitespace() {
        let mut h = harness(vec![text("first"), text("second")]);
        spawn(&mut h.fleet, &h.repo);
        next_event(&h.queue);
        h.fleet.record(1, WorkerEvent::Finished);
        assert_eq!(h.fleet.resume(" 1 "), "worker 1 resumed");
        assert_eq!(resume_id("\t1\n"), Ok(1));
    }
}
