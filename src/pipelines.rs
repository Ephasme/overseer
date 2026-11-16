//! User-authored Rhai pipelines: `~/.overseer/pipelines/<name>.rhai`, loaded once at startup.
//!
//! Rhai supplies the language (`if`, `while`, functions, expressions); Overseer supplies a narrow
//! host SDK for agent control, guarded commands and the file tools. A pipeline runs inside a
//! dedicated child agent and dies with the session, so nothing here is durable.
//!
//! Every run is bounded three ways: an operation count, a wall clock, and the data sizes the
//! hardened engine allows. The module resolver is dummy and `eval` is disabled, so a script reaches
//! no file, no module and no network except through the host functions below.

use crate::config;
use crate::conflict;
use crate::edit;
use crate::mcp;
use crate::profiles;
use crate::prompt;
use crate::skills;
use crate::swarm::{self, AgentEvent, Event, SpawnArgs, State, Swarm};
use crate::tools;
use crate::url;
use rhai::{Dynamic, Engine, EvalAltResult, ImmutableString, Position, Scope, module_resolvers::DummyModuleResolver};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, PoisonError, Weak,
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
};
use std::thread;
use std::time::{Duration, Instant};

/// Where the user keeps one pipeline per file, under their home.
pub(crate) const ROOT: &str = ".overseer/pipelines";

/// Operations one run may execute before it is stopped.
pub(crate) const MAX_OPERATIONS: u64 = 10_000_000;

/// Wall clock one run may take, in seconds.
pub(crate) const MAX_SECONDS: u64 = 3_600;

/// How often the watchdog and the waiting host functions look at the run flag.
pub(crate) const WAIT_POLL: Duration = Duration::from_millis(50);

/// Longest string one script may build.
pub(crate) const MAX_STRING_BYTES: usize = 64 * 1024;

/// Longest array one script may build.
pub(crate) const MAX_ARRAY: usize = 10_000;

/// Largest map one script may build.
pub(crate) const MAX_MAP: usize = 10_000;

/// Deepest call stack one script may reach.
pub(crate) const MAX_CALL_LEVELS: usize = 64;

/// Expression nesting: `(expressions, functions)`.
pub(crate) const MAX_EXPR_DEPTHS: (usize, usize) = (64, 32);

/// Most variables one script may hold.
pub(crate) const MAX_VARIABLES: usize = 256;

/// Most functions one script may define.
pub(crate) const MAX_FUNCTIONS: usize = 64;

/// The `//! requires:` line a pipeline must open with.
const HEADER: &str = "//! requires:";

const HEADER_ERROR: &str = "first non-empty line must be //! requires: <tool>[, <tool>...]";

/// The grant names a `//! requires:` header may name. `report`, `fail` and `sleep_ms` are always
/// available, as are the host function names (`run`, `read_text`, `write_text`, …), so a header
/// never declares them.
pub(crate) const SDK_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "spawn_agent",
    "message_agent",
    "control_agent",
    "list_agents",
    "list_repos",
    "start_pipeline",
    "list_pipelines",
    "free_spawn",
];

/// The two budgets one run is held to; tests pass smaller ones.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) operations: u64,
    pub(crate) seconds: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { operations: MAX_OPERATIONS, seconds: MAX_SECONDS }
    }
}

/// One pipeline as its file states it.
#[derive(Debug, Clone)]
pub(crate) struct Pipeline {
    pub(crate) name: String,
    pub(crate) source: String,
    /// The SDK tools its header declares; every one is preflighted before the run creates anything.
    pub(crate) requires: Vec<String>,
}

/// Every pipeline under the user root, with the files that could not be loaded and why.
#[derive(Debug, Default)]
pub(crate) struct Catalogue {
    pipelines: Vec<Pipeline>,
    by_name: HashMap<String, usize>,
    errors: HashMap<String, String>,
}

impl Catalogue {
    /// Every `*.rhai` directly under the user root; a missing or unreadable root is no pipeline.
    ///
    /// A file is loaded once, like a profile: editing one takes a restart. A malformed file is
    /// recorded under its name and reported when that name is asked for, never at startup.
    pub(crate) fn load(home: &Path) -> Self {
        let Ok(entries) = fs::read_dir(home.join(ROOT)) else { return Self::default() };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rhai"))
            .filter(|path| fs::metadata(path).is_ok_and(|meta| meta.is_file()))
            .collect();
        files.sort();
        let mut pipelines = Vec::new();
        let mut errors = HashMap::new();
        for path in files {
            let name = path.file_stem().map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
            match load_one(&path, &name) {
                Ok(pipeline) => pipelines.push(pipeline),
                Err(error) => {
                    errors.insert(name.clone(), format!("pipeline {name}: {error}"));
                }
            }
        }
        pipelines.sort_by(|a, b| a.name.cmp(&b.name));
        let by_name = pipelines.iter().enumerate().map(|(index, pipeline)| (pipeline.name.clone(), index)).collect();
        Self { pipelines, by_name, errors }
    }

    /// The pipeline `name` asks for, the recorded reason its file was refused, or an unknown name.
    pub(crate) fn get(&self, name: &str) -> Result<&Pipeline, String> {
        if let Some(index) = self.by_name.get(name)
            && let Some(pipeline) = self.pipelines.get(*index)
        {
            return Ok(pipeline);
        }
        if let Some(error) = self.errors.get(name) {
            return Err(error.clone());
        }
        Err(format!("unknown pipeline {name}; pipelines are {}", self.names()))
    }

    /// The valid names, comma-separated, in load order; `none` when the directory holds none.
    pub(crate) fn names(&self) -> String {
        if self.pipelines.is_empty() {
            return "none".into();
        }
        self.pipelines.iter().map(|pipeline| pipeline.name.as_str()).collect::<Vec<&str>>().join(", ")
    }
}

/// One file, or why it is not a pipeline: bad name, unreadable, bad header, or a syntax error.
fn load_one(path: &Path, name: &str) -> Result<Pipeline, String> {
    if !is_name(name) {
        return Err(format!("name must be 1 to 64 characters of a-z, 0-9, _ and -, got {name:?}"));
    }
    let source = fs::read_to_string(path).map_err(|error| config::io_error(path.display(), &error))?;
    let requires = parse_header(&source)?;
    // Compiling now catches syntax errors at load; an unknown function call is reported by the run,
    // where the host functions are registered.
    hardened_engine(&Limits::default()).compile(&source).map_err(|error| error.to_string())?;
    Ok(Pipeline { name: name.to_string(), source, requires })
}

/// A pipeline name: 1 to 64 bytes, starting with an ASCII letter or digit, then letters, digits,
/// `_` and `-` only. Spelled out rather than pulled from a regex: one rule, no dependency.
fn is_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let usable = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_' || *byte == b'-';
    match bytes.split_first() {
        Some((first, rest)) => (first.is_ascii_lowercase() || first.is_ascii_digit()) && rest.iter().all(usable),
        None => false,
    }
}

/// The tools the first non-empty line declares, or why the header is unusable.
fn parse_header(source: &str) -> Result<Vec<String>, String> {
    let Some(line) = source.lines().find(|line| !line.trim().is_empty()) else {
        return Err(HEADER_ERROR.into());
    };
    let Some(rest) = line.trim().strip_prefix(HEADER) else {
        return Err(HEADER_ERROR.into());
    };
    let rest = rest.trim();
    let mut requires: Vec<String> = Vec::new();
    if rest.is_empty() {
        return Ok(requires);
    }
    for entry in rest.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(format!("empty entry in {HEADER} {rest}"));
        }
        if !SDK_TOOLS.contains(&entry) {
            return Err(format!("unknown tool {entry:?} in {HEADER}; SDK tools are {}", SDK_TOOLS.join(", ")));
        }
        if requires.iter().any(|known| known == entry) {
            return Err(format!("duplicate requirement {entry:?}"));
        }
        requires.push(entry.to_string());
    }
    Ok(requires)
}

/// The one engine a pipeline is compiled and run with: no module resolver, no `eval`, no printing,
/// and every budget set. `Engine::new` installs `FileModuleResolver`, which would let a script
/// import files, so the resolver is replaced before anything else.
pub(crate) fn hardened_engine(limits: &Limits) -> Engine {
    let mut engine = Engine::new();
    engine.set_module_resolver(DummyModuleResolver::new());
    engine.disable_symbol("eval");
    engine.on_print(|_| {});
    engine.on_debug(|_, _, _| {});
    engine.set_max_operations(limits.operations);
    engine.set_max_call_levels(MAX_CALL_LEVELS);
    engine.set_max_expr_depths(MAX_EXPR_DEPTHS.0, MAX_EXPR_DEPTHS.1);
    engine.set_max_string_size(MAX_STRING_BYTES);
    engine.set_max_array_size(MAX_ARRAY);
    engine.set_max_map_size(MAX_MAP);
    engine.set_max_variables(MAX_VARIABLES);
    engine.set_max_functions(MAX_FUNCTIONS);
    // Zero imported modules: the first `import` fails, whatever resolver is installed.
    engine.set_max_modules(0);
    engine
}

/// The Rhai error a host failure becomes: the user reads it as the pipeline's failure line.
///
/// Boxed because every registered closure returns `Result<_, Box<EvalAltResult>>`, which is what
/// Rhai's native-function bridge expects.
#[allow(clippy::unnecessary_box_returns)]
fn runtime_error(message: String) -> Box<EvalAltResult> {
    Box::new(EvalAltResult::ErrorRuntime(Dynamic::from(message), Position::NONE))
}

/// A script argument that must index an agent or count milliseconds: negative and oversized values
/// are refused rather than wrapped.
fn count(value: i64, what: &str) -> Result<usize, Box<EvalAltResult>> {
    usize::try_from(value).map_err(|_| runtime_error(format!("{what} must be a non-negative integer, got {value}")))
}

/// What a pipeline may do to the tree and the filesystem. One implementation ships (`SwarmHost`);
/// tests script a fake, so the runner is proven without a model or a process.
pub(crate) trait Host: Clone + 'static {
    fn report(&self, text: &str);
    fn run(&self, command: &str) -> Result<String, String>;
    fn read_text(&self, path: &str) -> Result<String, String>;
    fn write_text(&self, path: &str, content: &str) -> Result<String, String>;
    fn list_repos(&self) -> Result<String, String>;
    fn list_agents(&self) -> Result<String, String>;
    fn start_agent(&self, task: &str, profile: &str, path: Option<&str>, free: bool) -> Result<usize, String>;
    fn message_agent(&self, id: usize, text: &str, urgent: bool) -> Result<String, String>;
    fn agent_status(&self, id: usize) -> Result<String, String>;
    fn wait_agent(&self, id: usize) -> Result<String, String>;
    fn control_agent(&self, id: usize, action: &str) -> Result<String, String>;
    fn start_pipeline(&self, name: &str) -> Result<usize, String>;
    fn list_pipelines(&self) -> String;
    fn fail(&self, text: &str) -> Result<(), String>;
    fn sleep_ms(&self, ms: u64) -> Result<(), String>;
}

/// Runs one pipeline against `host`, bounded by `limits`.
///
/// `cancel` is the owning agent's stop flag; `run_flag` is shared with the host so a running `bash`
/// is killed and `sleep_ms` stops within one [`WAIT_POLL`] of either the stop or the deadline. The
/// watchdog sets it from outside, which is the only bound a script cannot ignore.
pub(crate) fn execute<H: Host>(
    pipeline: &Pipeline,
    host: &H,
    limits: &Limits,
    cancel: &Arc<AtomicBool>,
    run_flag: &Arc<AtomicBool>,
) -> Result<String, String> {
    // The watchdog and the host share these; the caller keeps its own handles.
    let cancel = Arc::clone(cancel);
    let run_flag = Arc::clone(run_flag);
    let mut engine = hardened_engine(limits);
    let timed_out = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs(limits.seconds);
    let watchdog = {
        let cancel = Arc::clone(&cancel);
        let run_flag = Arc::clone(&run_flag);
        let timed_out = Arc::clone(&timed_out);
        thread::spawn(move || {
            // The run flag ends the watchdog too: `execute` sets it once the script returned, so
            // the join below never waits out the whole deadline.
            while !cancel.load(Ordering::SeqCst) && !run_flag.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    timed_out.store(true, Ordering::SeqCst);
                    break;
                }
                thread::sleep(WAIT_POLL);
            }
            run_flag.store(true, Ordering::SeqCst);
        })
    };
    // Rhai's own interrupt: the progress callback fires between operations, so a loop that never
    // calls a host function still stops.
    let progress = Arc::clone(&run_flag);
    engine.on_progress(move |_| progress.load(Ordering::SeqCst).then_some(Dynamic::UNIT));

    let last = Arc::new(Mutex::new(String::new()));
    {
        let host = host.clone();
        let last = Arc::clone(&last);
        engine.register_fn("report", move |text: ImmutableString| {
            host.report(text.as_str());
            *last.lock().unwrap_or_else(PoisonError::into_inner) = text.to_string();
        });
    }
    {
        let host = host.clone();
        engine.register_fn("run", move |command: ImmutableString| -> Result<String, Box<EvalAltResult>> {
            host.run(command.as_str()).map_err(runtime_error)
        });
    }
    {
        let host = host.clone();
        engine.register_fn("read_text", move |path: ImmutableString| -> Result<String, Box<EvalAltResult>> {
            host.read_text(path.as_str()).map_err(runtime_error)
        });
    }
    {
        let host = host.clone();
        engine.register_fn(
            "write_text",
            move |path: ImmutableString, content: ImmutableString| -> Result<String, Box<EvalAltResult>> {
                host.write_text(path.as_str(), content.as_str()).map_err(runtime_error)
            },
        );
    }
    {
        let host = host.clone();
        engine.register_fn("list_repos", move || -> Result<String, Box<EvalAltResult>> {
            host.list_repos().map_err(runtime_error)
        });
    }
    {
        let host = host.clone();
        engine.register_fn("list_agents", move || -> Result<String, Box<EvalAltResult>> {
            host.list_agents().map_err(runtime_error)
        });
    }
    {
        let host = host.clone();
        engine.register_fn(
            "start_agent",
            move |task: ImmutableString,
                  profile: ImmutableString,
                  path: ImmutableString,
                  free: bool|
                  -> Result<i64, Box<EvalAltResult>> {
                let path = (!path.is_empty()).then(|| path.to_string());
                let id =
                    host.start_agent(task.as_str(), profile.as_str(), path.as_deref(), free).map_err(runtime_error)?;
                i64::try_from(id).map_err(|_| runtime_error(format!("agent id {id} does not fit an integer")))
            },
        );
    }
    {
        let host = host.clone();
        engine.register_fn(
            "message_agent",
            move |id: i64, text: ImmutableString, urgent: bool| -> Result<String, Box<EvalAltResult>> {
                host.message_agent(count(id, "agent id")?, text.as_str(), urgent).map_err(runtime_error)
            },
        );
    }
    {
        let host = host.clone();
        engine.register_fn("agent_status", move |id: i64| -> Result<String, Box<EvalAltResult>> {
            host.agent_status(count(id, "agent id")?).map_err(runtime_error)
        });
    }
    {
        let host = host.clone();
        engine.register_fn("wait_agent", move |id: i64| -> Result<String, Box<EvalAltResult>> {
            host.wait_agent(count(id, "agent id")?).map_err(runtime_error)
        });
    }
    {
        let host = host.clone();
        engine.register_fn(
            "control_agent",
            move |id: i64, action: ImmutableString| -> Result<String, Box<EvalAltResult>> {
                host.control_agent(count(id, "agent id")?, action.as_str()).map_err(runtime_error)
            },
        );
    }
    {
        let host = host.clone();
        engine.register_fn("start_pipeline", move |name: ImmutableString| -> Result<i64, Box<EvalAltResult>> {
            let id = host.start_pipeline(name.as_str()).map_err(runtime_error)?;
            i64::try_from(id).map_err(|_| runtime_error(format!("agent id {id} does not fit an integer")))
        });
    }
    {
        let host = host.clone();
        engine.register_fn("list_pipelines", move || host.list_pipelines());
    }
    {
        let host = host.clone();
        engine.register_fn("fail", move |text: ImmutableString| -> Result<(), Box<EvalAltResult>> {
            host.fail(text.as_str()).map_err(runtime_error)
        });
    }
    {
        let host = host.clone();
        engine.register_fn("sleep_ms", move |ms: i64| -> Result<(), Box<EvalAltResult>> {
            host.sleep_ms(
                u64::try_from(ms)
                    .map_err(|_| runtime_error(format!("sleep_ms needs a non-negative integer, got {ms}")))?,
            )
            .map_err(runtime_error)
        });
    }

    let compiled = engine.compile(&pipeline.source);
    let evaluated =
        compiled.as_ref().ok().map(|ast| engine.eval_ast_with_scope::<Dynamic>(&mut Scope::new(), ast).map(|_| ()));
    run_flag.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    if timed_out.load(Ordering::SeqCst) {
        return Err(format!("pipeline {} timed out after {}s", pipeline.name, limits.seconds));
    }
    if cancel.load(Ordering::SeqCst) {
        return Err("pipeline cancelled".into());
    }
    match (compiled, evaluated) {
        (Err(error), _) => Err(format!("pipeline {} failed: {error}", pipeline.name)),
        (Ok(_), Some(Err(error))) if matches!(error.as_ref(), EvalAltResult::ErrorTooManyOperations(..)) => {
            Err(format!("pipeline {} failed: operation limit exceeded", pipeline.name))
        }
        (Ok(_), Some(Err(error))) => Err(format!("pipeline {} failed: {error}", pipeline.name)),
        (Ok(_), _) => {
            let text = last.lock().unwrap_or_else(PoisonError::into_inner).clone();
            Ok(if text.is_empty() { format!("pipeline {} completed", pipeline.name) } else { text })
        }
    }
}

/// The shipping host: one pipeline agent's view of the tree, its files and its commands.
///
/// Every guarded call goes through the same policy and timeout path an agent's does, so a script
/// cannot reach a tool its policy refuses, and its `bash` is killed when the agent is stopped.
#[derive(Clone)]
pub(crate) struct SwarmHost {
    pub(crate) swarm: Weak<Mutex<Swarm>>,
    pub(crate) id: usize,
    pub(crate) working_dir: PathBuf,
    pub(crate) policy: tools::Policy,
    pub(crate) skills: Arc<skills::Catalogue>,
    pub(crate) profiles: Arc<profiles::Catalogue>,
    pub(crate) mcp: Arc<mcp::Registry>,
    /// The user's approval gate, shared with every agent.
    pub(crate) approvals: Arc<crate::approvals::Gate>,
    pub(crate) config: Arc<config::Config>,
    pub(crate) remaining_depth: usize,
    pub(crate) sender: Sender<Event>,
    /// Set by the watchdog or by the owner's stop: a waiting host call returns at once.
    pub(crate) run_flag: Arc<AtomicBool>,
    pub(crate) conflicts: Arc<Mutex<conflict::Registry>>,
    /// The lines this pipeline's reads returned, so `edit` refuses a stale anchor.
    pub(crate) seen: Arc<Mutex<edit::Store>>,
}

impl SwarmHost {
    fn tree(&self) -> Result<Arc<Mutex<Swarm>>, String> {
        self.swarm.upgrade().ok_or_else(|| "the session ended".to_string())
    }

    /// One file tool under this pipeline's policy, with the same cancel flag an agent passes.
    fn guarded(&self, tool: &str, arguments: &str) -> Result<String, String> {
        // Held for the whole call: the tree must outlive the handlers that reach through the weak.
        let _tree = self.tree()?;
        // The pipeline host has no `edit` tool, so only bash and write_file are gated; a call the
        // policy refuses errors inside `execute_guarded` and never prompts.
        if matches!(tool, tools::BASH | tools::WRITE_FILE) && self.policy.refuse(tool, arguments).is_none() {
            self.gate(tool, arguments)?;
        }
        let weak = self.swarm.clone();
        let scope = url::Scope {
            caller: self.id,
            swarm: Some(&weak),
            config: &self.config,
            working_dir: &self.working_dir,
            policy: &self.policy,
            mcp: &self.mcp,
            remaining_depth: self.remaining_depth,
        };
        let context =
            tools::Context { skills: &self.skills, profiles: &self.profiles, mcp: &self.mcp, scope: Some(&scope) };
        let mut conflicts = self.conflicts.lock().unwrap_or_else(PoisonError::into_inner);
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let result = tools::execute_guarded(
            &self.working_dir,
            tool,
            arguments,
            &self.policy,
            Some(self.run_flag.as_ref()),
            &mut conflicts,
            &mut seen,
            &context,
        );
        match result.strip_prefix("error: ") {
            Some(reason) => Err(reason.to_string()),
            None => Ok(result),
        }
    }

    /// The approval gate for this host's gated calls, keyed like an agent's.
    fn gate(&self, tool: &str, arguments: &str) -> Result<(), String> {
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let display = swarm::describe(tool, arguments);
        let key = if tool == tools::BASH {
            let command = args.get("command").and_then(serde_json::Value::as_str).unwrap_or_default();
            command.trim().to_string()
        } else {
            let path = args.get("path").and_then(serde_json::Value::as_str).unwrap_or_default();
            path.to_string()
        };
        self.approvals.admit(self.id, tool, &key, &display, Some(self.run_flag.as_ref()), &self.sender)
    }

    /// One tree tool, dispatched exactly as an agent's own call is.
    fn tree_tool(&self, name: &str, arguments: &serde_json::Value) -> Result<String, String> {
        let swarm = self.tree()?;
        match Swarm::tool(&swarm, self.id, name, &arguments.to_string()) {
            Some(result) => result,
            None => Err(format!("{name} is not a tree tool")),
        }
    }
}

impl Host for SwarmHost {
    fn report(&self, text: &str) {
        let _ = self.sender.send(Event::Agent { id: self.id, kind: AgentEvent::Reply, text: text.to_string() });
    }

    fn run(&self, command: &str) -> Result<String, String> {
        self.guarded(tools::BASH, &json!({"command": command}).to_string())
    }

    fn read_text(&self, path: &str) -> Result<String, String> {
        self.guarded(tools::READ_FILE, &json!({"path": path}).to_string())
    }

    fn write_text(&self, path: &str, content: &str) -> Result<String, String> {
        self.guarded(tools::WRITE_FILE, &json!({"path": path, "content": content}).to_string())
    }

    fn list_repos(&self) -> Result<String, String> {
        let swarm = self.tree()?;
        Ok(Swarm::list_repos(&swarm))
    }

    fn list_agents(&self) -> Result<String, String> {
        let swarm = self.tree()?;
        Ok(swarm::lock(&swarm).list(self.id))
    }

    fn start_agent(&self, task: &str, profile: &str, path: Option<&str>, free: bool) -> Result<usize, String> {
        let swarm = self.tree()?;
        let args = SpawnArgs {
            task: task.to_string(),
            profile: profile.to_string(),
            path: path.filter(|path| !path.is_empty()).map(str::to_string),
            free,
        };
        Swarm::spawn(&swarm, self.id, args)
    }

    fn message_agent(&self, id: usize, text: &str, urgent: bool) -> Result<String, String> {
        self.tree_tool(prompt::MESSAGE_AGENT, &json!({"id": id, "text": text, "urgent": urgent}))
    }

    fn agent_status(&self, id: usize) -> Result<String, String> {
        let swarm = self.tree()?;
        swarm::lock(&swarm).agent_state(self.id, id).map(|state| state.word().to_string())
    }

    fn wait_agent(&self, id: usize) -> Result<String, String> {
        let swarm = self.tree()?;
        loop {
            if self.run_flag.load(Ordering::SeqCst) {
                return Err("pipeline interrupted".into());
            }
            let state = swarm::lock(&swarm).agent_state(self.id, id)?;
            match state {
                State::Done | State::Closed | State::Idle | State::Archived => {
                    return swarm::lock(&swarm).agent_output(self.id, id);
                }
                State::Stopped => return Err(format!("{} stopped", swarm::lock(&swarm).label(id))),
                State::Failed => return Err(format!("{} failed", swarm::lock(&swarm).label(id))),
                State::Paused | State::Working => thread::sleep(WAIT_POLL),
            }
        }
    }

    fn control_agent(&self, id: usize, action: &str) -> Result<String, String> {
        self.tree_tool(prompt::CONTROL_AGENT, &json!({"id": id, "action": action}))
    }

    fn start_pipeline(&self, name: &str) -> Result<usize, String> {
        let swarm = self.tree()?;
        Swarm::start_pipeline(&swarm, self.id, name)
    }

    fn list_pipelines(&self) -> String {
        match self.tree() {
            Ok(swarm) => swarm::lock(&swarm).pipeline_names(),
            Err(error) => error,
        }
    }

    fn fail(&self, text: &str) -> Result<(), String> {
        Err(text.to_string())
    }

    fn sleep_ms(&self, ms: u64) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if self.run_flag.load(Ordering::SeqCst) {
                return Err("pipeline interrupted".into());
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            thread::sleep((deadline - now).min(WAIT_POLL));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that records what the script reported and answers every other call with a constant.
    #[derive(Clone, Default)]
    struct Fake {
        reports: Arc<Mutex<Vec<String>>>,
    }

    impl Host for Fake {
        fn report(&self, text: &str) {
            self.reports.lock().unwrap().push(text.to_string());
        }
        fn run(&self, command: &str) -> Result<String, String> {
            Ok(command.to_string())
        }
        fn read_text(&self, path: &str) -> Result<String, String> {
            Ok(path.to_string())
        }
        fn write_text(&self, path: &str, _: &str) -> Result<String, String> {
            Ok(path.to_string())
        }
        fn list_repos(&self) -> Result<String, String> {
            Ok(String::new())
        }
        fn list_agents(&self) -> Result<String, String> {
            Ok(String::new())
        }
        fn start_agent(&self, _: &str, _: &str, _: Option<&str>, _: bool) -> Result<usize, String> {
            Ok(2)
        }
        fn message_agent(&self, _: usize, _: &str, _: bool) -> Result<String, String> {
            Ok(String::new())
        }
        fn agent_status(&self, _: usize) -> Result<String, String> {
            Ok("done".into())
        }
        fn wait_agent(&self, _: usize) -> Result<String, String> {
            Ok(String::new())
        }
        fn control_agent(&self, _: usize, _: &str) -> Result<String, String> {
            Ok(String::new())
        }
        fn start_pipeline(&self, _: &str) -> Result<usize, String> {
            Ok(3)
        }
        fn list_pipelines(&self) -> String {
            "fixture".into()
        }
        fn fail(&self, text: &str) -> Result<(), String> {
            Err(text.to_string())
        }
        fn sleep_ms(&self, _: u64) -> Result<(), String> {
            Ok(())
        }
    }

    fn pipeline(source: &str) -> Pipeline {
        Pipeline { name: "fixture".into(), source: source.into(), requires: Vec::new() }
    }

    fn run_with(source: &str, limits: Limits, cancel: bool) -> (Result<String, String>, Vec<String>) {
        let host = Fake::default();
        let result = execute(
            &pipeline(source),
            &host,
            &limits,
            &Arc::new(AtomicBool::new(cancel)),
            &Arc::new(AtomicBool::new(false)),
        );
        let reports = host.reports.lock().unwrap().clone();
        (result, reports)
    }

    #[test]
    fn a_while_loop_reports_in_order() {
        let (result, reports) = run_with(
            "//! requires:\nlet i = 0;\nwhile i < 3 { i += 1; report(\"step \" + i); }\n",
            Limits::default(),
            false,
        );
        assert_eq!(reports, ["step 1", "step 2", "step 3"]);
        assert_eq!(result, Ok("step 3".to_string()));
    }

    #[test]
    fn an_infinite_loop_hits_the_operation_limit() {
        let limits = Limits { operations: 10_000, seconds: 5 };
        let (result, _) = run_with("//! requires:\nloop { }\n", limits, false);
        assert!(result.unwrap_err().contains("operation limit exceeded"));
    }

    #[test]
    fn a_cancelled_run_stops() {
        let (result, _) = run_with("//! requires:\nreport(\"x\");\n", Limits::default(), true);
        assert_eq!(result, Err("pipeline cancelled".to_string()));
    }

    #[test]
    fn a_header_is_required_and_tools_must_be_known() {
        assert!(parse_header("report(\"x\");").is_err(), "a missing header is refused");
        assert!(parse_header("//! requires: bash, spawn_agent").is_ok(), "a known pair parses");
        assert!(parse_header("//! requires: bash, bash").is_err(), "a duplicate is refused");
        assert!(parse_header("//! requires: not_a_tool").is_err(), "an unknown tool is refused");
        assert_eq!(parse_header("//! requires:").unwrap(), Vec::<String>::new(), "an empty list is legal");
    }

    #[test]
    fn an_import_cannot_read_a_file() {
        let (result, _) = run_with("//! requires:\nimport \"x\" as x;\n", Limits::default(), false);
        let error = result.unwrap_err();
        assert!(error.contains("failed"), "{error:?}");
    }

    #[test]
    fn the_engine_disables_eval() {
        assert!(hardened_engine(&Limits::default()).is_symbol_disabled("eval"));
    }
}
