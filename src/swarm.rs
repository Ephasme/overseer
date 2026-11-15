use crate::agent::{INTERRUPTED, Turn};
use crate::config;
use crate::conflict;
use crate::edit;
use crate::llm::{Llm, Message, StreamLlm, StreamStep, ToolCall};
use crate::mcp;
use crate::prompt;
use crate::repos;
use crate::skills;
use crate::tools;
use serde::Deserialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{
    Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Locks the tree, ignoring a poisoned lock: one panicked agent must not wedge the others.
pub(crate) fn lock(swarm: &Arc<Mutex<Swarm>>) -> MutexGuard<'_, Swarm> {
    swarm.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Serialises agent spawns. A spawn starts its thread with the tree lock released, and this keeps
/// the id it validated free until the agent is in the tree.
static SPAWNING: Mutex<()> = Mutex::new(());

fn spawning() -> MutexGuard<'static, ()> {
    SPAWNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Longest one-line summary of a tool call.
const PROGRESS_WIDTH: usize = 100;

/// Longest task preview in `list_workers`.
const TASK_PREVIEW_WIDTH: usize = 80;

/// Message `/resume` sends: the worker keeps its history, so its task is already in it.
const CONTINUE: &str = "continue";

/// How long a worker waits on its inbox before looking for an urgent message or a stop.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// Arguments of each overseer tool. `deny_unknown_fields` turns a wrong key into an error naming the right ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    path: Option<String>,
    task: String,
    read_only: Option<bool>,
    deny_tools: Option<Vec<String>>,
    allow_commands: Option<Vec<String>>,
    deny_commands: Option<Vec<String>>,
}

impl SpawnArgs {
    fn policy(&self) -> tools::Policy {
        tools::Policy::default().set(
            self.read_only,
            self.deny_tools.clone(),
            self.allow_commands.clone(),
            self.deny_commands.clone(),
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    id: usize,
    text: String,
    #[serde(default)]
    urgent: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlArgs {
    id: usize,
    action: Action,
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Pause,
    Resume,
    Stop,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    text: String,
}

/// What Overseer may do to a worker at any moment: hold it, end it, or preempt its turn.
#[derive(Default)]
struct Control {
    gate: Mutex<Gate>,
    wake: Condvar,
    /// Set to abandon the running turn; the worker clears it when it starts an instruction.
    interrupted: AtomicBool,
    policy: Mutex<tools::Policy>,
}

#[derive(Default)]
struct Gate {
    paused: bool,
    stopped: bool,
    /// Urgent instructions, read before the inbox so urgency beats the backlog.
    urgent: VecDeque<String>,
}

impl Control {
    fn lock(&self) -> MutexGuard<'_, Gate> {
        self.gate.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn paused(&self) -> bool {
        self.lock().paused
    }

    fn pause(&self) {
        self.lock().paused = true;
    }

    fn unpause(&self) {
        self.lock().paused = false;
        self.wake.notify_all();
    }

    /// Ends the worker's turn and its thread: the next instruction never comes.
    fn stop(&self) {
        let mut gate = self.lock();
        gate.stopped = true;
        gate.urgent.clear();
        self.interrupted.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    /// Delivers `text` ahead of the backlog and cuts the running turn short.
    fn urgent(&self, text: String) {
        let mut gate = self.lock();
        gate.urgent.push_back(text);
        self.interrupted.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    fn flag(&self) -> &AtomicBool {
        &self.interrupted
    }

    fn policy(&self) -> tools::Policy {
        self.policy.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_policy(&self, policy: tools::Policy) {
        *self.policy.lock().unwrap_or_else(PoisonError::into_inner) = policy;
    }

    /// The next instruction to run, or `None` once the worker is stopped.
    ///
    /// Urgent messages come first, then the inbox. A pause holds the inbox but never an urgent
    /// message: an order that interrupts a turn also outranks the pause.
    ///
    /// The interrupt flag is cleared here, under the gate that sets it, at the dequeue: the order it
    /// announced is the one about to run. Clearing it a statement later would wipe an order that
    /// arrived in between, and the turn would run on after the overseer was told it was cut short.
    fn next(&self, inbox: &Receiver<String>) -> Option<String> {
        // Read from an inbox that was not paused yet: hold it rather than run it behind the pause.
        let mut held: Option<String> = None;
        loop {
            {
                let mut gate = self.lock();
                if gate.stopped {
                    return None;
                }
                if let Some(text) = gate.urgent.pop_front() {
                    self.interrupted.store(false, Ordering::SeqCst);
                    return Some(text);
                }
                if gate.paused {
                    drop(self.wake.wait(gate).unwrap_or_else(PoisonError::into_inner));
                    continue;
                }
                if let Some(text) = held.take() {
                    self.interrupted.store(false, Ordering::SeqCst);
                    return Some(text);
                }
            }
            match inbox.recv_timeout(IDLE_POLL) {
                Ok(text) => held = Some(text),
                Err(RecvTimeoutError::Timeout) => {}
                // A closed inbox means a stop, which also sets `stopped`; nothing more will come.
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}
/// One worker tool call, as the user sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Progress {
    /// A tool call started; `summary` is one line of its arguments.
    Started { call: usize, tool: String, summary: String },
    /// The same call returned after `secs`, with what its result is worth showing.
    Finished { call: usize, secs: f64, preview: tools::Preview },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AgentEvent {
    /// A progress note; it does not end the instruction it belongs to.
    Reply,
    Finished,
    Failed,
    /// An Overseer order cut the instruction short: neither an answer nor a failure.
    Interrupted,
}

impl AgentEvent {
    pub(crate) fn verb(self) -> &'static str {
        match self {
            AgentEvent::Reply => "says",
            AgentEvent::Finished => "finished",
            AgentEvent::Failed => "failed",
            AgentEvent::Interrupted => "interrupted",
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
    /// User typed `/schemes`: which internal URIs resolve, without a model call.
    Schemes,
    /// User typed `/mcp`: the server list, handled without a model call.
    Mcp,
    /// User typed `/login <server>`: the argument, handled without a model call.
    Login(String),
    Agent {
        id: usize,
        kind: AgentEvent,
        text: String,
    },
    /// One agent's model call is about to start: the transcript opens a new block for the root.
    TurnStart {
        id: usize,
    },
    /// Agent tool activity: shown to the user, never fed to the model.
    Progress {
        id: usize,
        progress: Progress,
    },
    /// Agent model output as it streams: shown to the user, never fed to the model.
    Delta {
        id: usize,
        text: String,
    },
}

impl Event {
    /// Maps one trimmed input line to an event; `None` for a blank line.
    ///
    /// Both frontends parse prompts through this, so `exit` and `/resume` cannot drift apart.
    pub(crate) fn from_line(line: &str) -> Option<Self> {
        if line.is_empty() {
            return None;
        }
        if line == "exit" {
            return Some(Event::Exit);
        }
        if line == "/schemes" {
            return Some(Event::Schemes);
        }
        if let Some(arg) = line.strip_prefix("/resume") {
            return Some(Event::Resume(arg.to_string()));
        }
        if line == "/mcp" {
            return Some(Event::Mcp);
        }
        if let Some(arg) = line.strip_prefix("/login") {
            return Some(Event::Login(arg.trim().to_string()));
        }
        Some(Event::User(line.to_string()))
    }
}

/// One worker as the panels show it.
#[allow(clippy::struct_excessive_bools)] // four independent flags, each shown on its own.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentView {
    pub(crate) id: usize,
    /// The agent that owns this one; `None` for the root.
    pub(crate) parent: Option<usize>,
    /// Delegations below the root: the root is 0.
    pub(crate) depth: usize,
    pub(crate) dir: PathBuf,
    /// The instruction the agent was started with.
    pub(crate) task: String,
    /// Instructions sent but not yet finished.
    pub(crate) pending: usize,
    pub(crate) failed: bool,
    pub(crate) finished: bool,
    /// Its owner stopped it: its thread is ending and it takes no more work.
    pub(crate) stopped: bool,
    /// Its owner paused it: it holds every queued instruction.
    pub(crate) paused: bool,
    /// Its tools refuse every write.
    pub(crate) read_only: bool,
    /// How long the current instruction has been running.
    pub(crate) busy_for: Option<Duration>,
}

/// Sends a progress line the user sees, without waking the overseer model.
fn progress(sender: &Sender<Event>, id: usize, progress: Progress) {
    let _ = sender.send(Event::Progress { id, progress });
}

/// The arguments `describe` reads: one field per tool. Everything else is skipped rather than
/// deserialised into a `Value` — a `write_file` content is as large as the file.
#[derive(Deserialize, Default)]
struct Described {
    path: Option<String>,
    task: Option<String>,
    text: Option<String>,
    command: Option<String>,
    action: Option<String>,
    id: Option<u64>,
    #[serde(default)]
    urgent: bool,
}

/// A string field of the arguments, or `""` when absent.
fn field(value: Option<&String>) -> &str {
    value.map_or("", String::as_str)
}

/// Its first line, so a multi-line instruction stays one row.
fn first_line(value: Option<&String>) -> &str {
    field(value).lines().next().unwrap_or_default()
}

/// What a tool call acts on: the transcript header and the worker panel both show it.
///
/// An instruction the overseer sends a worker is kept whole, so the user can read what was
/// actually said; every other summary is one capped line.
pub(crate) fn describe(name: &str, arguments: &str) -> String {
    let args: Described = serde_json::from_str(arguments).unwrap_or_default();
    let line = match name {
        prompt::SPAWN_AGENT => {
            let path = field(args.path.as_ref());
            let dir = if path.is_empty() { "owner's dir" } else { path };
            format!("{dir} · {}", field(args.task.as_ref()))
        }
        prompt::MESSAGE_AGENT => {
            let urgent = if args.urgent { "urgent " } else { "" };
            format!("#{urgent}{} · {}", args.id.unwrap_or_default(), field(args.text.as_ref()))
        }
        prompt::CONTROL_AGENT => format!("#{} · {}", args.id.unwrap_or_default(), field(args.action.as_ref())),
        prompt::REPLY => first_line(args.text.as_ref()).to_string(),
        tools::BASH => first_line(args.command.as_ref()).to_string(),
        tools::READ_FILE | tools::WRITE_FILE | tools::EDIT => field(args.path.as_ref()).to_string(),
        _ => String::new(),
    };
    if matches!(name, prompt::SPAWN_AGENT | prompt::MESSAGE_AGENT) {
        return line;
    }
    tools::shorten(&line, PROGRESS_WIDTH)
}

/// One agent in the tree. Ids are never reused, so a stale order can never reach a new agent.
struct Agent {
    id: usize,
    parent: Option<usize>,
    /// Delegations below the root: the root is 0.
    depth: usize,
    working_dir: PathBuf,
    /// The instruction the agent was started with; follow-up messages do not replace it.
    initial_task: String,
    /// Instructions sent but not yet finished. A `Reply` does not count.
    pending: usize,
    failed: bool,
    /// Its owner stopped it; the entry stays as a record and the inbox is gone.
    stopped: bool,
    control: Arc<Control>,
    inbox: Option<Sender<String>>,
    /// When the current instruction started; `None` once the agent is idle.
    busy_since: Option<Instant>,
    /// The agent thread. Its exit is the authoritative liveness signal.
    handle: JoinHandle<()>,
}

impl Agent {
    fn finished(&self) -> bool {
        self.handle.is_finished()
    }
}

/// The whole tree, shared by every agent thread: whoever holds the lock may spawn, message or stop.
///
/// A worker thread reaches it through a `Weak` (`spawn_thread`): the tree owns every worker's inbox,
/// so a strong handle back from the worker would keep the pair alive after the session left.
pub(crate) struct Swarm {
    llm: Arc<dyn Llm>,
    roots: Vec<PathBuf>,
    home: PathBuf,
    log_dir: PathBuf,
    sender: Sender<Event>,
    /// Delegations allowed below the root.
    max_depth: usize,
    /// Every agent, indexed by `id - 1`.
    agents: Vec<Agent>,
    /// Every agent shares the servers the process connected; a late one reaches the next turn.
    mcp: Arc<mcp::Registry>,
}

impl Swarm {
    /// The agent the user talks to.
    pub(crate) const ROOT: usize = 1;

    pub(crate) fn new(
        llm: Arc<dyn Llm>,
        roots: Vec<PathBuf>,
        home: PathBuf,
        log_dir: PathBuf,
        sender: Sender<Event>,
        max_depth: usize,
        mcp: Arc<mcp::Registry>,
    ) -> Self {
        Self { llm, roots, home, log_dir, sender, max_depth, agents: Vec::new(), mcp }
    }

    /// The tree as the frontends draw it.
    pub(crate) fn snapshot(&self) -> Vec<AgentView> {
        self.agents
            .iter()
            .map(|agent| AgentView {
                id: agent.id,
                parent: agent.parent,
                depth: agent.depth,
                dir: agent.working_dir.clone(),
                task: agent.initial_task.clone(),
                pending: agent.pending,
                failed: agent.failed,
                finished: agent.finished(),
                stopped: agent.stopped,
                paused: agent.control.paused(),
                read_only: agent.control.policy().read_only,
                busy_for: agent.busy_since.map(|since| since.elapsed()),
            })
            .collect()
    }

    /// Whether any agent is doing work: a paused one holds its instruction by order, so it does not
    /// keep the drain alive.
    pub(crate) fn busy(&self) -> bool {
        self.agents.iter().any(|agent| agent.pending > 0 && !agent.finished() && !agent.control.paused())
    }

    /// Updates an agent's status from one of its events. Call before handing the event to its owner.
    pub(crate) fn finish_turn(&mut self, id: usize, kind: AgentEvent) {
        let Some(agent) = self.agent_mut(id) else { return };
        if kind != AgentEvent::Reply {
            agent.pending = agent.pending.saturating_sub(1);
            if agent.pending == 0 {
                agent.busy_since = None;
            }
            agent.failed = kind == AgentEvent::Failed;
        }
    }

    /// Whether `caller` may command `target`: strictly below it in the tree, never itself.
    fn owns(&self, caller: usize, target: usize) -> bool {
        let mut current = self.agent(target).and_then(|agent| agent.parent);
        while let Some(id) = current {
            if id == caller {
                return true;
            }
            current = self.agent(id).and_then(|agent| agent.parent);
        }
        false
    }

    /// Everything strictly below `id`.
    fn descendants(&self, id: usize) -> Vec<usize> {
        let mut found = Vec::new();
        let mut frontier = vec![id];
        while let Some(parent) = frontier.pop() {
            for agent in &self.agents {
                if agent.parent == Some(parent) {
                    found.push(agent.id);
                    frontier.push(agent.id);
                }
            }
        }
        found
    }

    /// The agents `id` owns right now, for its prompt and its roster.
    fn children(&self, id: usize) -> Vec<usize> {
        self.agents.iter().filter(|agent| agent.parent == Some(id)).map(|agent| agent.id).collect()
    }

    /// Why `caller` may not address `target`, or `None` when it may.
    fn may_address(&self, caller: usize, target: usize) -> Option<String> {
        if self.agent(target).is_none() {
            return Some(format!("no agent {target}"));
        }
        if target == caller {
            return Some(format!("agent {target} is you: an agent commands its own children, not itself"));
        }
        if !self.owns(caller, target) {
            return Some(format!("agent {target} is not under your command"));
        }
        None
    }

    /// `/resume <id>`: the user owns every agent, so any id works.
    pub(crate) fn resume(&mut self, arg: &str) -> String {
        let result = resume_id(arg).and_then(|id| self.deliver_to(id, CONTINUE, true, false).map(|_| id));
        match result {
            Ok(id) => format!("agent {id} resumed"),
            Err(e) => format!("error: {e}; agent ids: {}", self.ids()),
        }
    }

    /// Ids that exist, so an error can name the ones `/resume` accepts.
    fn ids(&self) -> String {
        if self.agents.is_empty() {
            return "none".into();
        }
        self.agents.iter().map(|agent| agent.id.to_string()).collect::<Vec<String>>().join(", ")
    }

    /// The one place a 1-based id becomes a 0-based index.
    fn agent_mut(&mut self, id: usize) -> Option<&mut Agent> {
        id.checked_sub(1).and_then(|index| self.agents.get_mut(index))
    }

    fn agent(&self, id: usize) -> Option<&Agent> {
        id.checked_sub(1).and_then(|index| self.agents.get(index))
    }

    /// Starts the agent the user talks to: id 1, depth 0, no parent and no first instruction. Its
    /// rights are empty: nobody owns it, so nothing could restrict it.
    ///
    /// The log and the thread start with the tree lock released, as in `spawn`.
    pub(crate) fn start_root(swarm: &Arc<Mutex<Swarm>>, dir: PathBuf) -> Result<usize, String> {
        let _reserved = spawning();
        let (id, log_path, control, llm, sender) = {
            let tree = lock(swarm);
            if !tree.agents.is_empty() {
                return Err("the root is already running".to_string());
            }
            let id = Swarm::ROOT;
            (
                id,
                tree.log_dir.join(format!("agent-{id}.log")),
                Arc::new(Control::default()),
                Arc::clone(&tree.llm),
                tree.sender.clone(),
            )
        };
        let (inbox, handle) =
            spawn_thread(llm, sender, id, 0, None, &dir, Arc::clone(&control), None, &log_path, swarm)?;
        lock(swarm).agents.push(Agent {
            id,
            parent: None,
            depth: 0,
            working_dir: dir,
            initial_task: String::new(),
            pending: 0,
            failed: false,
            stopped: false,
            control,
            inbox: Some(inbox),
            busy_since: None,
            handle,
        });
        Ok(id)
    }

    /// The user's own instruction: an instruction to the root, counted like any other.
    pub(crate) fn tell_root(&mut self, text: &str) -> Result<String, String> {
        if self.agent(Swarm::ROOT).is_none() {
            return Err("the root is not running".to_string());
        }
        self.deliver_to(Swarm::ROOT, text, true, false)?;
        Ok("sent to agent 1".to_string())
    }

    /// Where a report goes: its owner, or `None` for the root, whose reader is the user.
    pub(crate) fn deliver_target(&self, child: usize) -> Option<usize> {
        self.agent(child).and_then(|agent| agent.parent)
    }

    /// Hands a child's report to its owner, as a new instruction.
    pub(crate) fn deliver(&mut self, child: usize, kind: AgentEvent, text: &str) {
        let Some(parent) = self.deliver_target(child) else { return };
        let _ = self.deliver_to(parent, &format!("[agent {child} {}] {text}", kind.verb()), false, false);
    }

    /// Runs one tree tool for `caller`; `None` when `name` is not one, so the caller falls through
    /// to the file tools.
    pub(crate) fn tool(
        swarm: &Arc<Mutex<Swarm>>,
        caller: usize,
        name: &str,
        arguments: &str,
    ) -> Option<Result<String, String>> {
        // A banned tree tool is refused here too, so a call that bypasses the turn loop still obeys
        // the caller's rights: denying `spawn_agent` holds whatever path reaches this function.
        let tree_tool = matches!(
            name,
            prompt::SPAWN_AGENT
                | prompt::MESSAGE_AGENT
                | prompt::CONTROL_AGENT
                | prompt::LIST_AGENTS
                | prompt::LIST_REPOS
        );
        if tree_tool {
            let refused = {
                let tree = lock(swarm);
                tree.agent(caller).and_then(|agent| agent.control.policy().refuse(name, arguments))
            };
            if let Some(refusal) = refused {
                return Some(Err(refusal));
            }
        }
        match name {
            prompt::SPAWN_AGENT => Some(
                tools::parse_args::<SpawnArgs>(arguments)
                    .and_then(|args| Swarm::spawn(swarm, caller, args).map(|id| format!("agent {id} started"))),
            ),
            prompt::MESSAGE_AGENT => {
                Some(tools::parse_args::<MessageArgs>(arguments).and_then(|args| lock(swarm).message(caller, &args)))
            }
            prompt::CONTROL_AGENT => {
                Some(tools::parse_args::<ControlArgs>(arguments).and_then(|args| lock(swarm).control(caller, &args)))
            }
            prompt::LIST_AGENTS => Some(tools::parse_args::<NoArgs>(arguments).map(|_| lock(swarm).list(caller))),
            prompt::LIST_REPOS => Some(tools::parse_args::<NoArgs>(arguments).map(|_| Swarm::list_repos(swarm))),
            _ => None,
        }
    }

    /// Starts one agent below `caller`. The new thread reaches the same tree through `swarm`.
    ///
    /// The policy is merged under the tree lock, before the thread exists: nothing can change it
    /// afterwards. The log file is opened and the thread started with that lock released, because
    /// both block and every other agent's dispatch, the session's events and `snapshot()` wait on it.
    fn spawn(swarm: &Arc<Mutex<Swarm>>, caller: usize, args: SpawnArgs) -> Result<usize, String> {
        let _reserved = spawning();
        let (depth, dir, home, id, log_path, control, llm, sender) = {
            let tree = lock(swarm);
            let parent = tree.agent(caller).ok_or_else(|| format!("no agent {caller}"))?;
            let depth = parent.depth.saturating_add(1);
            if depth > tree.max_depth {
                return Err(format!(
                    "depth limit: an agent below {caller} would be at depth {depth}, past max_depth {}",
                    tree.max_depth
                ));
            }
            let control = Arc::new(Control::default());
            control.set_policy(parent.control.policy().inherit(&args.policy()));
            let dir = args.path.clone().unwrap_or_else(|| parent.working_dir.display().to_string());
            let id = tree.agents.len() + 1;
            (
                depth,
                dir,
                tree.home.clone(),
                id,
                tree.log_dir.join(format!("agent-{id}.log")),
                control,
                Arc::clone(&tree.llm),
                tree.sender.clone(),
            )
        };
        let working_dir = config::resolve_dir(&dir, &home)?;
        let (inbox, handle) = spawn_thread(
            llm,
            sender,
            id,
            depth,
            Some(caller),
            &working_dir,
            Arc::clone(&control),
            Some(args.task.clone()),
            &log_path,
            swarm,
        )?;
        lock(swarm).agents.push(Agent {
            id,
            parent: Some(caller),
            depth,
            working_dir,
            initial_task: args.task,
            pending: 1,
            failed: false,
            stopped: false,
            control,
            inbox: Some(inbox),
            busy_since: Some(Instant::now()),
            handle,
        });
        Ok(id)
    }

    fn message(&mut self, caller: usize, args: &MessageArgs) -> Result<String, String> {
        if let Some(refusal) = self.may_address(caller, args.id) {
            return Err(refusal);
        }
        self.deliver_to(args.id, &args.text, false, args.urgent)
    }

    fn control(&mut self, caller: usize, args: &ControlArgs) -> Result<String, String> {
        if let Some(refusal) = self.may_address(caller, args.id) {
            return Err(refusal);
        }
        let id = args.id;
        match args.action {
            Action::Stop => {
                let mut stopped = self.descendants(id);
                stopped.push(id);
                for target in &stopped {
                    if let Some(agent) = self.agent_mut(*target) {
                        agent.control.stop();
                        agent.inbox = None;
                        agent.stopped = true;
                        // Nothing will run the instructions still queued, so nothing will ever
                        // finish them: the counter and the busy mark would stay set for good.
                        agent.pending = 0;
                        agent.busy_since = None;
                    }
                }
                let below = stopped.len().saturating_sub(1);
                Ok(if below == 0 {
                    format!("agent {id} stopped")
                } else {
                    format!("agent {id} stopped, with {below} agent(s) below it")
                })
            }
            Action::Pause => {
                let agent = self.agent_mut(id).ok_or_else(|| format!("no agent {id}"))?;
                if agent.finished() {
                    return Err(format!("agent {id} is gone"));
                }
                agent.control.pause();
                Ok(format!("agent {id} paused; it finishes the current instruction, then holds every queued one"))
            }
            Action::Resume => {
                let agent = self.agent_mut(id).ok_or_else(|| format!("no agent {id}"))?;
                if agent.finished() {
                    return Err(format!("agent {id} is gone"));
                }
                if agent.stopped {
                    return Err(format!("agent {id} is stopped; spawn a new one with the rights you want"));
                }
                agent.control.unpause();
                Ok(format!("agent {id} resumed"))
            }
        }
    }

    /// Queues `text` for one agent, counting it as pending work.
    fn deliver_to(&mut self, id: usize, text: &str, clear_failure: bool, urgent: bool) -> Result<String, String> {
        let agent = self.agent_mut(id).ok_or_else(|| format!("no agent {id}"))?;
        if agent.finished() {
            return Err(format!("agent {id} is gone"));
        }
        if agent.stopped {
            return Err(format!("agent {id} is stopped"));
        }
        if urgent {
            agent.control.urgent(text.to_string());
        } else {
            let inbox = agent.inbox.as_ref().ok_or_else(|| format!("agent {id} is stopped"))?;
            inbox.send(text.to_string()).map_err(|_| format!("agent {id} is gone"))?;
        }
        let busy = agent.pending > 0;
        if !busy {
            agent.busy_since = Some(Instant::now());
        }
        agent.pending += 1;
        if clear_failure {
            agent.failed = false;
        }
        Ok(if urgent && busy {
            format!("interrupted agent {id}; it reads your message now")
        } else if agent.pending > 1 {
            format!("queued for agent {id}; it will read it after its current instruction")
        } else {
            format!("sent to agent {id}")
        })
    }

    /// The agents `caller` may address, itself included, deepest last.
    fn list(&self, caller: usize) -> String {
        let mut mine: Vec<&Agent> =
            self.agents.iter().filter(|agent| agent.id == caller || self.owns(caller, agent.id)).collect();
        if mine.is_empty() {
            return format!("no agent {caller}");
        }
        mine.sort_by_key(|agent| (agent.depth, agent.id));
        mine.iter().map(|agent| self.line(agent, caller)).collect::<Vec<String>>().join("\n")
    }

    /// One roster line: `agent 3 [depth 1 · working] /dir | task`, the caller and its rights marked.
    #[allow(clippy::unused_self)] // the roster reads as one block; the line reads no field of the tree.
    fn line(&self, agent: &Agent, caller: usize) -> String {
        let state = if agent.stopped {
            "stopped"
        } else if agent.finished() {
            // The thread is gone: a recorded failure, or work it will never finish, is a failure; a
            // worker with nothing outstanding ended cleanly.
            if agent.failed || agent.pending > 0 { "failed" } else { "done" }
        } else if agent.control.paused() {
            "paused"
        } else if agent.pending > 0 {
            "working"
        } else if agent.failed {
            "failed"
        } else {
            "idle"
        };
        let read_only = if agent.control.policy().read_only { " · read-only" } else { "" };
        let you = if agent.id == caller { " · you" } else { "" };
        let task: String = agent.initial_task.lines().next().unwrap_or("").chars().take(TASK_PREVIEW_WIDTH).collect();
        format!(
            "agent {} [depth {} · {state}{read_only}{you}] {} | {task}",
            agent.id,
            agent.depth,
            agent.working_dir.display()
        )
    }

    /// The repositories under the configured roots. The walk runs with the tree lock released: it
    /// recurses the whole filesystem, and every agent's dispatch waits on that lock.
    fn list_repos(swarm: &Arc<Mutex<Swarm>>) -> String {
        let configured = lock(swarm).roots.clone();
        let found = repos::find(&configured);
        if found.is_empty() {
            let roots: Vec<String> = configured.iter().map(|root| root.display().to_string()).collect();
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

/// Starts one agent thread: its inbox, and the handle proving it lives. `task` is the first
/// instruction; the root starts with none and waits for the user.
///
/// The tree is reached through a `Weak`: the tree holds this worker's inbox, so a strong handle back
/// would keep the pair alive for the life of the process, once the session has let the tree go.
#[allow(clippy::too_many_arguments)] // one call site each for the root and for a spawned agent.
fn spawn_thread(
    llm: Arc<dyn Llm>,
    sender: Sender<Event>,
    id: usize,
    depth: usize,
    parent: Option<usize>,
    working_dir: &Path,
    control: Arc<Control>,
    task: Option<String>,
    log_path: &Path,
    me: &Arc<Mutex<Swarm>>,
) -> Result<(Sender<String>, JoinHandle<()>), String> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| config::io_error(log_path.display(), &e))?;
    let (inbox, instructions) = mpsc::channel();
    let tree = Arc::downgrade(me);
    let stream_sender = sender.clone();
    let streamed = Arc::new(StreamLlm::new(
        llm,
        Arc::new(move |step| match step {
            StreamStep::Start => {
                let _ = stream_sender.send(Event::TurnStart { id });
            }
            StreamStep::Text(delta) => {
                let _ = stream_sender.send(Event::Delta { id, text: delta.to_string() });
            }
        }),
    ));
    let agent_dir = working_dir.to_path_buf();
    let handle = thread::spawn(move || {
        let mut log = log;
        let mut run = AgentRun {
            swarm: &tree,
            id,
            depth,
            parent,
            working_dir: &agent_dir,
            sender: &sender,
            log: &mut log,
            calls: 0,
            control: &control,
            conflicts: conflict::Registry::default(),
            seen: edit::Store::default(),
        };
        run.run(streamed.as_ref(), &instructions);
    });
    if let Some(task) = task {
        inbox.send(task).map_err(|_| format!("agent {id} could not be started"))?;
    }
    Ok((inbox, handle))
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

/// One agent thread: its place in the tree, where progress goes, its log and its orders.
///
/// The tree is held weakly: it owns this worker's inbox, and this worker's turn needs it, so a
/// strong handle here would keep both alive after the session let the tree go.
struct AgentRun<'a> {
    swarm: &'a Weak<Mutex<Swarm>>,
    id: usize,
    depth: usize,
    parent: Option<usize>,
    working_dir: &'a Path,
    sender: &'a Sender<Event>,
    log: &'a mut File,
    control: &'a Control,
    /// Conflict blocks this agent has read, by id: resolving one writes only its marker lines.
    conflicts: conflict::Registry,
    /// What this agent has read or written, by canonical path: `edit` refuses anything else.
    seen: edit::Store,
    /// Tool calls made so far, so each start pairs with its own result whatever interleaves.
    calls: usize,
}

impl AgentRun<'_> {
    /// Runs every instruction the agent receives until its owner stops it or the inbox closes.
    fn run(&mut self, llm: &dyn Llm, instructions: &Receiver<String>) {
        let control = self.control;
        let (home, mcp) = {
            let Some(swarm) = self.swarm.upgrade() else { return };
            let tree = lock(&swarm);
            (tree.home.clone(), Arc::clone(&tree.mcp))
        };
        // Discovered once per agent: a skill installed mid-session needs a new agent.
        let skills = skills::Catalogue::in_dir(&home, self.working_dir);
        let mut history: Vec<Message> = Vec::new();
        // Log writes are best effort: a full disk must not kill the agent.
        while let Some(input) = control.next(instructions) {
            let policy = control.policy();
            let (facts, remaining) = {
                let Some(swarm) = self.swarm.upgrade() else { return };
                let tree = lock(&swarm);
                let facts = prompt::AgentFacts {
                    id: self.id,
                    parent: self.parent,
                    depth: self.depth,
                    max_depth: tree.max_depth,
                    dir: self.working_dir,
                    policy: &policy,
                    children: tree.children(self.id),
                    skills: &skills,
                    mcp: mcp.as_ref(),
                    root: self.parent.is_none(),
                };
                (facts, tree.max_depth.saturating_sub(self.depth))
            };
            let prompt = prompt::agent_prompt(&facts);
            if let Some(first) = history.first_mut() {
                first.content = Some(prompt);
            } else {
                history.push(Message::system(prompt));
            }
            // Rebuilt per instruction: a server that connected after this agent started must reach it.
            let tool_defs = prompt::agent_tool_definitions(&mcp, &policy, remaining);
            let _ = writeln!(self.log, "> {input}");
            let result = {
                let context = tools::Context { skills: &skills, mcp: mcp.as_ref() };
                let mut turn = Turn {
                    llm,
                    tool_defs: &tool_defs,
                    execute: &mut |call: &ToolCall| self.execute(call, control, &context),
                    cancel: Some(control.flag()),
                };
                turn.run(&mut history, &input)
            };
            let (kind, text) = match result {
                Ok(answer) => (AgentEvent::Finished, answer),
                Err(e) if e.to_string() == INTERRUPTED => {
                    (AgentEvent::Interrupted, "your owner cut this turn short".to_string())
                }
                Err(e) => (AgentEvent::Failed, format!("{e:#}")),
            };
            let _ = writeln!(self.log, "= {kind:?}: {text}");
            let _ = self.sender.send(Event::Agent { id: self.id, kind, text });
        }
    }

    /// A tree tool's result, or `None` when `call` is not one. The tree is upgraded for the call
    /// only, so a worker whose session is gone cannot reach it and cannot keep it alive.
    fn tree_tool(&self, call: &ToolCall) -> Option<Result<String, String>> {
        let swarm = self.swarm.upgrade()?;
        Swarm::tool(&swarm, self.id, &call.function.name, &call.function.arguments)
    }

    /// Runs one tool call under the agent's policy, logging it and reporting its progress to the user.
    fn execute(&mut self, call: &ToolCall, control: &Control, context: &tools::Context<'_>) -> String {
        // The log keeps the capped summary, never the raw arguments: a `write_file` carries a whole
        // file, whose content would land on disk in plain text.
        let summary = describe(&call.function.name, &call.function.arguments);
        let _ = writeln!(self.log, "· {} ({} bytes) {summary}", call.function.name, call.function.arguments.len());
        let started = Instant::now();
        let seq = (call.function.name != prompt::REPLY).then(|| {
            let seq = self.calls;
            self.calls += 1;
            seq
        });
        if let Some(seq) = seq {
            progress(self.sender, self.id, Progress::Started { call: seq, tool: call.function.name.clone(), summary });
        }
        let policy = control.policy();
        // A banned tool is refused before any dispatch, so a denied `spawn_agent` reads as a ban and
        // not as a depth or ownership error. The four file tools are refused inside execute_guarded.
        if !matches!(call.function.name.as_str(), tools::BASH | tools::READ_FILE | tools::WRITE_FILE | tools::EDIT)
            && let Some(refusal) = policy.refuse(&call.function.name, &call.function.arguments)
        {
            let output = format!("error: {refusal}");
            let _ = writeln!(self.log, "{output}");
            if let Some(seq) = seq {
                progress(
                    self.sender,
                    self.id,
                    Progress::Finished {
                        call: seq,
                        secs: started.elapsed().as_secs_f64(),
                        preview: tools::preview(&call.function.name, &output),
                    },
                );
            }
            return output;
        }
        let output = if call.function.name == prompt::REPLY {
            match tools::parse_args::<ReplyArgs>(&call.function.arguments) {
                Ok(args) => {
                    let _ = self.sender.send(Event::Agent { id: self.id, kind: AgentEvent::Reply, text: args.text });
                    "sent to your owner".to_string()
                }
                Err(e) => format!("error: {e}"),
            }
        } else if let Some(result) = self.tree_tool(call) {
            tools::outcome(result)
        } else {
            tools::execute_guarded(
                self.working_dir,
                &call.function.name,
                &call.function.arguments,
                &policy,
                Some(control.flag()),
                &mut self.conflicts,
                &mut self.seen,
                context,
            )
        };
        let _ = writeln!(self.log, "{output}");
        if let Some(seq) = seq {
            let preview = tools::preview(&call.function.name, &output);
            let secs = started.elapsed().as_secs_f64();
            progress(self.sender, self.id, Progress::Finished { call: seq, secs, preview });
        }
        output
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::testing::{Scripted, call, text};
    use serde_json::{Value, json};
    use std::fs;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    struct Harness {
        swarm: Arc<Mutex<Swarm>>,
        queue: Receiver<Event>,
        repo: PathBuf,
        logs: tempfile::TempDir,
        root: tempfile::TempDir,
    }

    /// The next event that is not streamed model output: these tests assert agent outcomes.
    fn next_event(queue: &Receiver<Event>) -> Event {
        loop {
            match queue.recv_timeout(WAIT).unwrap() {
                Event::Delta { .. } | Event::TurnStart { .. } => {}
                event => return event,
            }
        }
    }

    /// The next agent lifecycle event, skipping streamed output and tool progress.
    fn next_agent_event(queue: &Receiver<Event>) -> Event {
        loop {
            match next_event(queue) {
                Event::Progress { .. } => {}
                event => return event,
            }
        }
    }

    /// The next tool event, skipping everything else an agent sends.
    fn next_progress(queue: &Receiver<Event>) -> Progress {
        loop {
            if let Event::Progress { progress, .. } = queue.recv_timeout(WAIT).unwrap() {
                return progress;
            }
        }
    }

    /// A tool call's outcome, waiting for its own result whatever interleaves.
    fn next_finished(queue: &Receiver<Event>) -> tools::Preview {
        loop {
            if let Progress::Finished { preview, .. } = next_progress(queue) {
                return preview;
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
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let swarm = Arc::new(Mutex::new(Swarm::new(
            llm,
            roots,
            root.path().to_path_buf(),
            logs.path().to_path_buf(),
            sender,
            3,
            mcp,
        )));
        Swarm::start_root(&swarm, root.path().to_path_buf()).unwrap();
        Harness { swarm, queue, repo, logs, root }
    }

    /// One tree tool call, as an agent makes it.
    fn tool(swarm: &Arc<Mutex<Swarm>>, caller: usize, name: &str, args: &Value) -> String {
        tools::outcome(
            Swarm::tool(swarm, caller, name, &args.to_string()).unwrap_or_else(|| Err(format!("unknown tool {name}"))),
        )
    }

    fn spawn(swarm: &Arc<Mutex<Swarm>>, caller: usize, dir: &Path, task: &str) -> String {
        tool(swarm, caller, prompt::SPAWN_AGENT, &json!({"path": dir, "task": task}))
    }

    fn list(swarm: &Arc<Mutex<Swarm>>, caller: usize) -> String {
        tool(swarm, caller, prompt::LIST_AGENTS, &json!({}))
    }

    #[test]
    fn from_line_classifies_prompts() {
        assert_eq!(Event::from_line(""), None);
        assert_eq!(Event::from_line("exit"), Some(Event::Exit));
        assert_eq!(Event::from_line("/resume 3"), Some(Event::Resume(" 3".into())));
        assert_eq!(Event::from_line("/schemes"), Some(Event::Schemes));
        assert_eq!(Event::from_line("/mcp"), Some(Event::Mcp));
        assert_eq!(Event::from_line("/login github"), Some(Event::Login("github".into())));
        assert_eq!(Event::from_line("hello"), Some(Event::User("hello".into())));
    }

    #[test]
    fn an_agent_tool_call_reports_what_it_ran_and_what_it_returned() {
        let h = harness(vec![call("t1", tools::BASH, &json!({"command": "printf 'a\\nb\\n'"})), text("done")]);
        h.swarm.lock().unwrap().tell_root("run").unwrap();
        assert_eq!(
            next_progress(&h.queue),
            Progress::Started { call: 0, tool: tools::BASH.into(), summary: "printf 'a\\nb\\n'".into() }
        );
        let Progress::Finished { call, secs, preview, .. } = next_progress(&h.queue) else {
            panic!("the call never reported a result");
        };
        assert_eq!(call, 0, "the result belongs to the call that started");
        assert!(secs >= 0.0);
        assert!(!preview.failed);
        assert_eq!(preview.head, "a\nb");
        assert_eq!(preview.status, "");
    }

    #[test]
    fn every_agent_tool_is_dispatched() {
        let h = harness(vec![]);
        let mcp = mcp::Registry::load(h.root.path());
        for definition in prompt::agent_tool_definitions(&mcp, &tools::Policy::default(), 3).as_array().unwrap() {
            let name = definition.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            // The built-ins and `reply` are not tree tools: they fall through to the turn loop.
            if let Some(result) = Swarm::tool(&h.swarm, Swarm::ROOT, name, &json!({}).to_string()) {
                assert!(
                    !tools::outcome(result).starts_with("error: unknown tool"),
                    "{name} is defined but not dispatched"
                );
            }
        }
    }

    #[test]
    fn list_repos_reports_git_directories_and_nothing_else() {
        let h = harness(vec![]);
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::LIST_REPOS, &json!({})),
            format!("no git repositories found under {}", fs::canonicalize(h.root.path()).unwrap().display())
        );
        fs::create_dir(h.repo.join(".git")).unwrap();
        fs::create_dir(h.root.path().join("plain")).unwrap();
        fs::create_dir_all(h.repo.join("nested/.git")).unwrap();
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::LIST_REPOS, &json!({})),
            fs::canonicalize(&h.repo).unwrap().display().to_string()
        );
    }

    #[test]
    fn describe_names_what_a_tool_call_acts_on() {
        assert_eq!(describe(prompt::SPAWN_AGENT, &json!({"task": "x"}).to_string()), "owner's dir · x");
        let nested = json!({"path": "/repo", "task": "a\nb"}).to_string();
        assert_eq!(describe(prompt::SPAWN_AGENT, &nested), "/repo · a\nb", "the whole task, newlines kept");
        let message = json!({"id": 4, "text": "one\ntwo"}).to_string();
        assert_eq!(describe(prompt::MESSAGE_AGENT, &message), "#4 · one\ntwo");
        let command = json!({"command": "cargo test\nls"}).to_string();
        assert_eq!(describe(tools::BASH, &command), "cargo test");
        assert_eq!(describe(prompt::LIST_REPOS, "{}"), "");
        assert_eq!(describe(tools::BASH, "not json"), "");
        let long = json!({"command": "x".repeat(200)}).to_string();
        let summary = describe(tools::BASH, &long);
        assert_eq!(summary.chars().count(), 101);
        assert!(summary.ends_with('…'));
        let long = json!({"id": 1, "text": "y".repeat(400)}).to_string();
        assert_eq!(describe(prompt::MESSAGE_AGENT, &long).chars().count(), 405, "no cap on an instruction");
    }

    #[test]
    fn an_agent_prompt_states_its_depth_tools_and_fixed_rights() {
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let policy = tools::Policy::default();
        let facts = prompt::AgentFacts {
            id: 2,
            parent: Some(1),
            depth: 1,
            max_depth: 3,
            dir: Path::new("/tmp"),
            policy: &policy,
            children: vec![3],
            skills: &skills,
            mcp: &mcp,
            root: false,
        };
        let rendered = prompt::agent_prompt(&facts);
        assert!(rendered.contains("You are agent 2 of a tree of identical agents"), "{rendered}");
        assert!(rendered.contains("at depth 1 and agents may be nested to depth 3"), "{rendered}");
        assert!(rendered.contains("Agent 1 started you and owns you"), "{rendered}");
        assert!(rendered.contains("Agents you own right now: agent 3"), "{rendered}");
        assert!(rendered.contains("Your rights are fixed"), "{rendered}");
        assert!(rendered.contains("Ownership is one way"), "{rendered}");
        for definition in prompt::agent_tool_definitions(&mcp, &policy, 2).as_array().unwrap() {
            let name = definition.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            assert!(rendered.contains(name), "{name} missing from the agent prompt");
        }
        let root_facts = prompt::AgentFacts { id: 1, parent: None, depth: 0, root: true, ..facts };
        assert!(
            prompt::agent_prompt(&root_facts).contains("The user talks to you directly"),
            "the root must know its answer is the user's"
        );
        // Non-root with no parent is a contradiction: the prompt must say so, not invent an owner.
        let orphan = prompt::AgentFacts { id: 5, root: false, ..root_facts };
        let orphan_prompt = prompt::agent_prompt(&orphan);
        assert!(!orphan_prompt.contains("Agent 0 started you"), "{orphan_prompt}");
        assert!(orphan_prompt.contains("Your owner is not recorded"), "{orphan_prompt}");
    }

    /// The tools array must hold only what the policy and the depth allow: a call that can only be
    /// refused teaches the model to waste turns.
    #[test]
    fn a_banned_or_impossible_tool_is_not_advertised() {
        let home = tempfile::tempdir().unwrap();
        let mcp = mcp::Registry::load(home.path());
        let advertised = |policy: &tools::Policy, remaining: usize| -> Vec<String> {
            prompt::agent_tool_definitions(&mcp, policy, remaining)
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| Some(tool.get("function")?.get("name")?.as_str()?.to_string()))
                .collect()
        };
        let banned = tools::Policy {
            deny_tools: vec![prompt::SPAWN_AGENT.into(), prompt::REPLY.into()],
            ..tools::Policy::default()
        };
        let names = advertised(&banned, 2);
        assert!(!names.contains(&prompt::SPAWN_AGENT.to_string()), "a banned spawn_agent was offered");
        assert!(!names.contains(&prompt::REPLY.to_string()), "a banned reply was offered");
        assert!(names.contains(&prompt::MESSAGE_AGENT.to_string()));
        let deepest = advertised(&tools::Policy::default(), 0);
        assert!(!deepest.contains(&prompt::SPAWN_AGENT.to_string()), "spawn_agent offered with no depth left");
        assert!(deepest.contains(&prompt::LIST_AGENTS.to_string()));
    }

    #[test]
    fn the_roots_answer_is_the_users() {
        let h = harness(vec![text("hello")]);
        assert_eq!(h.swarm.lock().unwrap().tell_root("hi").unwrap(), "sent to agent 1");
        let answer = Event::Agent { id: 1, kind: AgentEvent::Finished, text: "hello".into() };
        assert_eq!(next_event(&h.queue), answer);
    }

    #[test]
    fn a_reports_reach_its_owner_and_the_roots_are_the_users() {
        let h = harness(vec![text("child says done"), text("root says acknowledged")]);
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "do it"), "agent 2 started");
        let child = next_event(&h.queue);
        assert!(matches!(child, Event::Agent { id: 2, kind: AgentEvent::Finished, .. }), "{child:?}");
        h.swarm.lock().unwrap().deliver(2, AgentEvent::Finished, "child says done");
        let root = next_event(&h.queue);
        assert_eq!(root, Event::Agent { id: 1, kind: AgentEvent::Finished, text: "root says acknowledged".into() });
        assert_eq!(h.swarm.lock().unwrap().deliver_target(2), Some(1));
        assert_eq!(h.swarm.lock().unwrap().deliver_target(1), None, "the root reports to the user");
    }

    #[test]
    fn an_agent_commands_only_its_descendants() {
        let h = harness(vec![text("a"), text("b")]);
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "one"), "agent 2 started");
        assert_eq!(spawn(&h.swarm, 2, &h.repo, "two"), "agent 3 started");
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "sibling"), "agent 4 started");
        assert_eq!(
            tool(&h.swarm, 2, prompt::MESSAGE_AGENT, &json!({"id": 1, "text": "x"})),
            "error: agent 1 is not under your command"
        );
        assert_eq!(
            tool(&h.swarm, 2, prompt::MESSAGE_AGENT, &json!({"id": 2, "text": "x"})),
            "error: agent 2 is you: an agent commands its own children, not itself"
        );
        assert_eq!(
            tool(&h.swarm, 3, prompt::MESSAGE_AGENT, &json!({"id": 4, "text": "x"})),
            "error: agent 4 is not under your command"
        );
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::CONTROL_AGENT, &json!({"id": 3, "action": "pause"})),
            "agent 3 paused; it finishes the current instruction, then holds every queued one"
        );
    }

    #[test]
    fn the_depth_limit_counts_delegations_below_the_root() {
        let h = harness(vec![text("a"), text("b"), text("c"), text("d")]);
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "l1"), "agent 2 started");
        assert_eq!(spawn(&h.swarm, 2, &h.repo, "l2"), "agent 3 started");
        assert_eq!(spawn(&h.swarm, 3, &h.repo, "l3"), "agent 4 started");
        assert_eq!(
            spawn(&h.swarm, 4, &h.repo, "l4"),
            "error: depth limit: an agent below 4 would be at depth 4, past max_depth 3"
        );
        let listed = list(&h.swarm, Swarm::ROOT);
        assert!(listed.contains("agent 4 [depth 3 · "), "{listed}");
        assert!(listed.contains("agent 1 [depth 0 · "), "{listed}");
    }

    #[test]
    fn a_child_cannot_start_sub_agents_once_spawning_is_denied() {
        let h = harness(vec![text("a"), text("b")]);
        let started = tool(
            &h.swarm,
            Swarm::ROOT,
            prompt::SPAWN_AGENT,
            &json!({"path": h.repo, "task": "leaf", "deny_tools": ["spawn_agent"]}),
        );
        assert_eq!(started, "agent 2 started");
        assert_eq!(spawn(&h.swarm, 2, &h.repo, "nope"), "error: tool spawn_agent is banned for this agent");
        let listed = list(&h.swarm, Swarm::ROOT);
        assert!(listed.contains("agent 2 [depth 1 · "), "{listed}");
    }

    #[test]
    fn rights_come_from_the_owner_and_cannot_be_changed_later() {
        let h = harness(vec![text("a")]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, "restrict_agent", &json!({"id": 2, "read_only": true})),
            "error: unknown tool restrict_agent"
        );
        assert!(!list(&h.swarm, Swarm::ROOT).contains("read-only"), "{}", list(&h.swarm, Swarm::ROOT));
        // The only way to change rights: stop it, which stops everything below, and spawn a new one.
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::CONTROL_AGENT, &json!({"id": 2, "action": "stop"})),
            "agent 2 stopped"
        );
        assert_eq!(
            tool(
                &h.swarm,
                Swarm::ROOT,
                prompt::SPAWN_AGENT,
                &json!({"path": h.repo, "task": "two", "read_only": true})
            ),
            "agent 3 started"
        );
        assert!(
            list(&h.swarm, Swarm::ROOT).contains("agent 3 [depth 1 · working · read-only"),
            "{}",
            list(&h.swarm, Swarm::ROOT)
        );
    }

    #[test]
    fn a_workers_read_is_what_licenses_its_edit() {
        let h = harness(vec![
            call("r1", tools::READ_FILE, &json!({"path": "t.rs"})),
            call("e1", tools::EDIT, &json!({"path": "t.rs", "patch": "PUT 1.=1:\n+B\n"})),
            text("done"),
            call("e2", tools::EDIT, &json!({"path": "t.rs", "patch": "PUT 1.=1:\n+C\n"})),
            text("refused"),
        ]);
        fs::write(h.repo.join("t.rs"), "a\n").unwrap();
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "edit it"), "agent 2 started");
        assert_eq!(next_finished(&h.queue).status, "1 lines");
        let edited = next_finished(&h.queue);
        assert_eq!(edited.status, "+1/-1");
        assert_eq!(edited.head, "-1|a\n+1|B");
        assert!(
            matches!(next_agent_event(&h.queue), Event::Agent { id: 2, kind: AgentEvent::Finished, .. }),
            "the first worker must finish before the second starts"
        );
        // The second agent sees the same file, but its own store has never read it.
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "edit it again"), "agent 3 started");
        let refused = next_finished(&h.queue);
        assert!(refused.failed);
        assert!(refused.head.contains("read t.rs before editing it"), "{}", refused.head);
    }

    #[test]
    fn stopping_an_agent_stops_everything_below_it() {
        let h = harness(vec![text("a"), text("b"), text("c")]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        spawn(&h.swarm, 2, &h.repo, "two");
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::CONTROL_AGENT, &json!({"id": 2, "action": "stop"})),
            "agent 2 stopped, with 1 agent(s) below it"
        );
        let listed = list(&h.swarm, Swarm::ROOT);
        assert!(listed.contains("agent 2 [depth 1 · stopped"), "{listed}");
        assert!(listed.contains("agent 3 [depth 2 · stopped"), "{listed}");
    }

    #[test]
    fn a_reply_and_a_final_answer_reach_the_owner() {
        let h = harness(vec![call("c1", prompt::REPLY, &json!({"text": "halfway"})), text("done")]);
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "do it"), "agent 2 started");
        assert!(h.swarm.lock().unwrap().busy());
        let reply = Event::Agent { id: 2, kind: AgentEvent::Reply, text: "halfway".into() };
        assert_eq!(next_event(&h.queue), reply);
        let finished = Event::Agent { id: 2, kind: AgentEvent::Finished, text: "done".into() };
        assert_eq!(next_event(&h.queue), finished);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        assert!(!h.swarm.lock().unwrap().busy());
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · idle]"), "{}", list(&h.swarm, Swarm::ROOT));
        let log = fs::read_to_string(h.logs.path().join("agent-2.log")).unwrap();
        assert!(log.contains("> do it") && log.contains("· reply") && log.contains("= Finished: done"), "{log}");
    }

    #[test]
    fn message_reaches_existing_agent_history() {
        let h = harness(vec![text("first"), text("second")]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        next_event(&h.queue);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        let sent = tool(&h.swarm, Swarm::ROOT, prompt::MESSAGE_AGENT, &json!({"id": 2, "text": "more"}));
        assert_eq!(sent, "sent to agent 2");
        let finished = Event::Agent { id: 2, kind: AgentEvent::Finished, text: "second".into() };
        assert_eq!(next_event(&h.queue), finished);
    }

    #[test]
    fn failed_turn_is_reported() {
        let h = harness(vec![]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        let failed = Event::Agent { id: 2, kind: AgentEvent::Failed, text: "script exhausted".into() };
        assert_eq!(next_event(&h.queue), failed);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Failed);
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · failed]"), "{}", list(&h.swarm, Swarm::ROOT));
    }

    #[test]
    fn a_panicking_agent_does_not_keep_the_swarm_busy() {
        struct Panic;
        impl Llm for Panic {
            fn chat(&self, _: &[Message], _: &Value) -> anyhow::Result<Message> {
                panic!("boom")
            }
        }
        let h = harness_with(Arc::new(Panic));
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        let deadline = Instant::now() + WAIT;
        while h.swarm.lock().unwrap().busy() {
            assert!(Instant::now() < deadline, "a dead agent kept the swarm busy");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · failed]"), "{}", list(&h.swarm, Swarm::ROOT));
    }

    #[test]
    fn spawn_takes_any_directory_and_defaults_to_its_owner() {
        let h = harness(vec![text("a"), text("b")]);
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, elsewhere.path(), "one"), "agent 2 started");
        assert_eq!(tool(&h.swarm, Swarm::ROOT, prompt::SPAWN_AGENT, &json!({"task": "do it"})), "agent 3 started");
        // A spawn with no path lands in its owner's directory: the root's own temp dir, not `/tmp`.
        let owner = h.root.path().file_name().unwrap().to_string_lossy().into_owned();
        let listed = list(&h.swarm, Swarm::ROOT);
        assert!(listed.contains(&format!("{owner} | do it")), "{listed}");
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::MESSAGE_AGENT, &json!({"id": 7, "text": "x"})),
            "error: no agent 7"
        );
        assert!(
            tool(&h.swarm, Swarm::ROOT, prompt::SPAWN_AGENT, &json!({"repo": "/tmp", "task": "x"})).starts_with(
                "error: invalid arguments: unknown field `repo`, expected one of `path`, `task`, `read_only`, `deny_tools`, `allow_commands`, `deny_commands`"
            )
        );
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::SPAWN_AGENT, &json!({"path": "/nonexistent/x", "task": "x"})),
            "error: /nonexistent/x: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn resume_retasks_an_idle_agent() {
        let h = harness(vec![text("first"), text("second")]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        next_event(&h.queue);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        assert_eq!(h.swarm.lock().unwrap().resume("2"), "agent 2 resumed");
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · working]"), "{}", list(&h.swarm, Swarm::ROOT));
        let finished = Event::Agent { id: 2, kind: AgentEvent::Finished, text: "second".into() };
        assert_eq!(next_event(&h.queue), finished);
    }

    #[test]
    fn resume_clears_a_failed_agent() {
        let h = harness(vec![text("first"), text("second")]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        next_event(&h.queue);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Failed);
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · failed]"), "{}", list(&h.swarm, Swarm::ROOT));
        assert_eq!(h.swarm.lock().unwrap().resume("2"), "agent 2 resumed");
        let finished = Event::Agent { id: 2, kind: AgentEvent::Finished, text: "second".into() };
        assert_eq!(next_event(&h.queue), finished);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · idle]"), "{}", list(&h.swarm, Swarm::ROOT));
    }

    #[test]
    fn resume_rejects_bad_arguments_without_messaging() {
        let h = harness(vec![text("first")]);
        assert_eq!(h.swarm.lock().unwrap().resume("9"), "error: no agent 9; agent ids: 1");
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        next_event(&h.queue);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        assert_eq!(h.swarm.lock().unwrap().resume(""), "error: usage: /resume <id>; agent ids: 1, 2");
        assert_eq!(h.swarm.lock().unwrap().resume("x"), "error: not a worker id: x; agent ids: 1, 2");
        assert_eq!(h.swarm.lock().unwrap().resume("1 2"), "error: expected one worker id, got: 1 2; agent ids: 1, 2");
        assert!(!h.swarm.lock().unwrap().busy());
        while h.queue.recv_timeout(Duration::from_millis(100)).is_ok() {}
        assert!(h.queue.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn resume_tolerates_surrounding_whitespace() {
        let h = harness(vec![text("first"), text("second")]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        next_event(&h.queue);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        assert_eq!(h.swarm.lock().unwrap().resume(" 2 "), "agent 2 resumed");
        assert_eq!(resume_id("\t1\n"), Ok(1));
    }

    #[test]
    fn an_urgent_message_cuts_a_running_command_short() {
        let h = harness(vec![call("c1", tools::BASH, &json!({"command": "sleep 30"})), text("urgent done")]);
        assert_eq!(spawn(&h.swarm, Swarm::ROOT, &h.repo, "one"), "agent 2 started");
        let Progress::Started { .. } = next_progress(&h.queue) else { panic!("the command never started") };
        let sent = tool(&h.swarm, Swarm::ROOT, prompt::MESSAGE_AGENT, &json!({"id": 2, "text": "now", "urgent": true}));
        assert_eq!(sent, "interrupted agent 2; it reads your message now");
        let interrupted =
            Event::Agent { id: 2, kind: AgentEvent::Interrupted, text: "your owner cut this turn short".into() };
        assert_eq!(next_agent_event(&h.queue), interrupted);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Interrupted);
        let finished = Event::Agent { id: 2, kind: AgentEvent::Finished, text: "urgent done".into() };
        assert_eq!(next_event(&h.queue), finished);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        let log = fs::read_to_string(h.logs.path().join("agent-2.log")).unwrap();
        assert!(log.contains("interrupted by overseer"), "{log}");
        assert!(log.contains("> now"), "{log}");
    }

    #[test]
    fn pause_holds_the_next_instruction_until_resumed() {
        let h = harness(vec![text("first"), text("second")]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        next_event(&h.queue);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        let paused = tool(&h.swarm, Swarm::ROOT, prompt::CONTROL_AGENT, &json!({"id": 2, "action": "pause"}));
        assert!(paused.contains("paused"), "{paused}");
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · paused]"), "{}", list(&h.swarm, Swarm::ROOT));
        tool(&h.swarm, Swarm::ROOT, prompt::MESSAGE_AGENT, &json!({"id": 2, "text": "later"}));
        assert!(
            h.queue.recv_timeout(Duration::from_millis(300)).is_err(),
            "a paused agent ran the instruction it was sent"
        );
        let resumed = tool(&h.swarm, Swarm::ROOT, prompt::CONTROL_AGENT, &json!({"id": 2, "action": "resume"}));
        assert!(resumed.contains("resumed"), "{resumed}");
        let finished = Event::Agent { id: 2, kind: AgentEvent::Finished, text: "second".into() };
        assert_eq!(next_event(&h.queue), finished);
    }

    #[test]
    fn a_stopped_agent_takes_no_more_work() {
        let h = harness(vec![call("c1", tools::BASH, &json!({"command": "sleep 30"}))]);
        spawn(&h.swarm, Swarm::ROOT, &h.repo, "one");
        let Progress::Started { .. } = next_progress(&h.queue) else { panic!("the command never started") };
        let stopped = tool(&h.swarm, Swarm::ROOT, prompt::CONTROL_AGENT, &json!({"id": 2, "action": "stop"}));
        assert_eq!(stopped, "agent 2 stopped");
        assert!(list(&h.swarm, Swarm::ROOT).contains("agent 2 [depth 1 · stopped]"), "{}", list(&h.swarm, Swarm::ROOT));
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::MESSAGE_AGENT, &json!({"id": 2, "text": "x"})),
            "error: agent 2 is stopped"
        );
        assert_eq!(
            tool(&h.swarm, Swarm::ROOT, prompt::CONTROL_AGENT, &json!({"id": 2, "action": "resume"})),
            "error: agent 2 is stopped; spawn a new one with the rights you want"
        );
        let interrupted =
            Event::Agent { id: 2, kind: AgentEvent::Interrupted, text: "your owner cut this turn short".into() };
        assert_eq!(next_agent_event(&h.queue), interrupted);
        let deadline = Instant::now() + WAIT;
        while h.swarm.lock().unwrap().busy() {
            assert!(Instant::now() < deadline, "a stopped agent kept the swarm busy");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_spawned_policy_is_enforced_and_listed() {
        let h = harness(vec![call("w", tools::WRITE_FILE, &json!({"path": "x", "content": "y"})), text("done")]);
        let started = tool(
            &h.swarm,
            Swarm::ROOT,
            prompt::SPAWN_AGENT,
            &json!({"path": h.repo, "task": "write", "read_only": true, "deny_tools": ["bash"]}),
        );
        assert_eq!(started, "agent 2 started");
        let preview = next_finished(&h.queue);
        assert!(preview.failed, "a read-only agent's write_file went through");
        assert!(preview.head.contains("read-only"), "{preview:?}");
        next_event(&h.queue);
        h.swarm.lock().unwrap().finish_turn(2, AgentEvent::Finished);
        assert!(
            list(&h.swarm, Swarm::ROOT).contains("[depth 1 · idle · read-only]"),
            "{}",
            list(&h.swarm, Swarm::ROOT)
        );
    }
}
