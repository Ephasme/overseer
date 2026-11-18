use crate::approvals::Gate;
use crate::bounds;
use crate::config;
use crate::event::UiEvent;
use crate::llm::Models;
use crate::mcp;
use crate::names;
use crate::pipelines;
use crate::profiles;
use crate::session_store::{Restore, SessionLog, TreeRecord};
use crate::swarm::{AgentEvent, CompactionNotice, Event, Swarm, lock};
use crate::tools;
use crate::url;
use crate::version;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
/// How often the loop looks at the shutdown flag, so a frontend that returned cannot be waited on.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Routes events between the frontends and the tree; it never calls a model itself.
///
/// Blocking by design: the model client and every agent are blocking, so a frontend keeps this on its own thread.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub(crate) fn run(
    models: Arc<dyn Models>,
    profiles: Arc<profiles::Catalogue>,
    pipelines: Arc<pipelines::Catalogue>,
    cfg: &config::Config,
    log_dir: PathBuf,
    input: Sender<Event>,
    events: Receiver<Event>,
    ui: Sender<UiEvent>,
    mcp: Arc<mcp::Registry>,
    approvals: Arc<Gate>,
    shutdown: Arc<AtomicBool>,
    bounds: Arc<bounds::Bounds>,
    restore: Option<Restore>,
) -> Result<()> {
    let _ = ui.send(UiEvent::Notice(format!("{} · agent logs in {}", version::VERSION, log_dir.display())));
    let _ = ui.send(UiEvent::Approvals(approvals.tier()));
    // One session dir = one durable state: the tree records and each agent's committed history.
    let session_log = match SessionLog::open(&log_dir) {
        Ok(log) => log,
        Err(e) => {
            let _ = ui.send(UiEvent::Error(e.clone()));
            tools::kill_running();
            mcp.kill_all();
            let _ = ui.send(UiEvent::Done);
            return Err(anyhow::Error::msg(e));
        }
    };
    let (resumed_from, forked_from) = match &restore {
        Some(Restore::Resume(source)) => (Some(source.id.clone()), None),
        Some(Restore::Fork { source, .. }) => (None, Some(source.id.clone())),
        None => (None, None),
    };
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut log = session_log;
    log.write(&TreeRecord::Session { at, dir: cfg.dir.display().to_string(), resumed_from, forked_from });
    let swarm = Arc::new(Mutex::new(Swarm::new(
        models,
        profiles,
        pipelines,
        Arc::new(cfg.clone()),
        log_dir.clone(),
        input,
        Arc::clone(&mcp),
        Arc::clone(&approvals),
        Arc::clone(&bounds),
        Some(log),
    )));
    let started = match &restore {
        Some(restore) => Swarm::restore_tree(&swarm, restore).map(Some),
        None => Swarm::start_root(&swarm, &cfg.dir).map(|_| None),
    };
    match started {
        Ok(Some(line)) => {
            let _ = ui.send(UiEvent::Notice(line));
        }
        Ok(None) => {}
        Err(e) => {
            let _ = ui.send(UiEvent::Error(e.clone()));
            tools::kill_running();
            mcp.kill_all();
            let _ = ui.send(UiEvent::Done);
            // The picker never lists a session that holds nothing.
            let _ = std::fs::remove_dir_all(&log_dir);
            return Err(anyhow::Error::msg(e));
        }
    }
    let _ = ui.send(UiEvent::Agents(lock(&swarm).snapshot()));
    // What the frontend was last told the session spent: a charge that matches it has nothing new
    // to show, and this poll runs ten times a second.
    let (mut metered, mut billed_tokens) = (0.0_f64, 0_u64);
    // Set when stdin closes: the run ends at the first moment no agent is busy.
    let mut draining = false;
    // Set once a bound trips: one notice per session, not one per poll.
    let mut tripped = false;
    while !shutdown.load(Ordering::Relaxed) {
        let event = match events.recv_timeout(SHUTDOWN_POLL) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // The soft ceiling and the meter ride the same poll. A charge only lands when a call
        // returns, so a value that differs from the last one is one event per model call.
        if let Some(notice) = bounds.warning() {
            let _ = ui.send(UiEvent::Notice(notice));
        }
        let (cost, tokens) = (bounds.spent(), bounds.tokens());
        // Bits, not `==`: the comparison only asks whether a charge landed, never whether two
        // dollars are equal to some precision.
        if cost.to_bits() != metered.to_bits() || tokens != billed_tokens {
            metered = cost;
            billed_tokens = tokens;
            let _ = ui.send(UiEvent::Meter { cost, tokens, agents: bounds.agents() });
        }
        // The watchdog rides the same poll: a bound blown while agents are busy is caught here.
        if !tripped && let Some(reason) = bounds.exceeded() {
            tripped = true;
            let (agents, busy) = {
                let mut tree = lock(&swarm);
                tree.stop_all();
                (tree.snapshot(), tree.busy())
            };
            let _ = ui.send(UiEvent::Error(format!("session budget exceeded: {reason}; every agent stopped")));
            let _ = ui.send(UiEvent::Agents(agents));
            let _ = ui.send(UiEvent::Busy(busy));
            let unbilled = bounds.unbilled();
            if unbilled > 0 {
                let _ = ui.send(UiEvent::Notice(format!(
                    "{unbilled} model call(s) reported no token counts and were not billed"
                )));
            }
            // Same exit rule as every other busy-falsifying arm: with stdin closed the run ends
            // here, not after a poll that will never fire.
            if draining && !busy {
                break;
            }
        }
        let Some(event) = event else {
            continue;
        };
        match event {
            Event::Exit => break,
            Event::Eof => {
                draining = true;
                if lock(&swarm).busy() {
                    let _ = ui.send(UiEvent::Notice("draining: quits when no agent is busy".into()));
                    continue;
                }
                break;
            }
            Event::Schemes => {
                let _ = ui.send(UiEvent::Notice(url::report()));
            }
            Event::Mcp => {
                let _ = ui.send(UiEvent::Notice(mcp.summary()));
            }
            Event::Approve(arg) => {
                let line = if arg.is_empty() { "usage: /approve <server>".to_string() } else { mcp.approve(&arg) };
                let _ = ui.send(UiEvent::Notice(line));
            }
            Event::Login(arg) => {
                let line = if arg.is_empty() {
                    "usage: /login <server>".to_string()
                } else {
                    let notice = ui.clone();
                    mcp.login(&arg, &|url| {
                        let _ = notice.send(UiEvent::Notice(format!("open this URL to sign in: {url}")));
                    })
                };
                let _ = ui.send(UiEvent::Notice(line));
            }
            Event::Pipeline(arg) => {
                const USAGE: &str = "usage: /pipeline <name> | list | stop <id>";
                let words: Vec<&str> = arg.split_whitespace().collect();
                match words.as_slice() {
                    ["list"] => {
                        let names = lock(&swarm).pipeline_names();
                        let _ = ui.send(UiEvent::Notice(names));
                    }
                    ["stop", id] => match id.parse::<usize>() {
                        Ok(id) => {
                            let (result, agents, busy) = {
                                let mut tree = lock(&swarm);
                                let result = tree.stop_pipeline(id);
                                (result, tree.snapshot(), tree.busy())
                            };
                            let _ = ui.send(match result {
                                Ok(line) => UiEvent::Notice(line),
                                Err(line) => UiEvent::Error(line),
                            });
                            let _ = ui.send(UiEvent::Agents(agents));
                            let _ = ui.send(UiEvent::Busy(busy));
                            if draining && !busy {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = ui.send(UiEvent::Error(format!("not an agent id: {id}")));
                        }
                    },
                    [name] => {
                        // A pipeline agent never passes through `Turn`, so the bound cannot stop
                        // it: once the session is blown, starting one would run unbounded.
                        if tripped {
                            let _ = ui.send(UiEvent::Error("session budget exceeded: pipelines are refused".into()));
                        } else {
                            match Swarm::start_pipeline(&swarm, Swarm::ROOT, name) {
                                Ok(id) => {
                                    draining = false;
                                    let (label, agents, busy) = {
                                        let tree = lock(&swarm);
                                        (tree.label(id), tree.snapshot(), tree.busy())
                                    };
                                    let _ = ui.send(UiEvent::Notice(format!("pipeline {name} started as {label}")));
                                    let _ = ui.send(UiEvent::Agents(agents));
                                    let _ = ui.send(UiEvent::Busy(busy));
                                }
                                Err(line) => {
                                    let _ = ui.send(UiEvent::Error(line));
                                }
                            }
                        }
                    }
                    _ => {
                        let _ = ui.send(UiEvent::Notice(USAGE.into()));
                    }
                }
            }
            Event::Resume(arg) => {
                let (line, agents, busy) = {
                    let mut tree = lock(&swarm);
                    let line = tree.resume(&arg);
                    (line, tree.snapshot(), tree.busy())
                };
                let _ = ui.send(UiEvent::Notice(line));
                let _ = ui.send(UiEvent::Agents(agents));
                let _ = ui.send(UiEvent::Busy(busy));
            }
            Event::Compact(arg) => {
                let (result, agents) = {
                    let mut tree = lock(&swarm);
                    let result = tree.compact(&arg);
                    (result, tree.snapshot())
                };
                let _ = ui.send(match result {
                    Ok(line) => UiEvent::Notice(line),
                    Err(line) => UiEvent::Error(line),
                });
                let _ = ui.send(UiEvent::Agents(agents));
            }
            Event::Compaction { id, notice } => {
                let label = names::label(id, None);
                let event = match notice {
                    CompactionNotice::Compacted { before, after, over_band } => {
                        let mut line = format!("compacted {label}: {before} → {after} tokens");
                        if over_band {
                            line.push_str("; still above the recovery band");
                        }
                        UiEvent::Notice(line)
                    }
                    CompactionNotice::Skipped => UiEvent::Notice(format!("nothing older to summarize for {label}")),
                    CompactionNotice::Failed(error) => {
                        UiEvent::Error(format!("compaction failed for {label}: {error}"))
                    }
                };
                let _ = ui.send(event);
            }
            Event::Stop { id } => {
                let (result, agents, busy) = {
                    let mut tree = lock(&swarm);
                    let result = tree.stop(id);
                    (result, tree.snapshot(), tree.busy())
                };
                let _ = ui.send(match result {
                    Ok(line) => UiEvent::Notice(line),
                    Err(line) => UiEvent::Error(line),
                });
                let _ = ui.send(UiEvent::Agents(agents));
                let _ = ui.send(UiEvent::Busy(busy));
                if draining && !busy {
                    break;
                }
            }
            Event::Rename { id, name } => {
                let (line, agents) = {
                    let mut tree = lock(&swarm);
                    let line = tree.rename(id, &name);
                    (line, tree.snapshot())
                };
                let _ = ui.send(UiEvent::Notice(line));
                let _ = ui.send(UiEvent::Agents(agents));
            }
            Event::ApprovalRequest { id, request, tool, call } => {
                let _ = ui.send(UiEvent::ApprovalRequest { agent: id, request, tool, call });
            }
            Event::Resolve { request, choice } => {
                approvals.answer(request, choice);
            }
            Event::ApprovalWithdrawn { request } => {
                let _ = ui.send(UiEvent::ApprovalWithdrawn { request });
            }
            Event::CycleTier => {
                let tier = approvals.cycle();
                let _ = ui.send(UiEvent::Approvals(tier));
            }
            Event::Progress { id, progress } => {
                let _ = ui.send(UiEvent::Progress { id, progress });
            }
            Event::Delta { id, text } => {
                let _ = ui.send(UiEvent::AgentDelta { id, text });
            }
            Event::Thought { id, text } => {
                let _ = ui.send(UiEvent::AgentThought { id, text });
            }
            Event::Out { id, text } => {
                let _ = ui.send(UiEvent::Out { id, text });
            }
            Event::TurnStart { id } => {
                let _ = ui.send(UiEvent::Boundary { id });
            }
            Event::Instruction { id, text } => {
                // The lead shows the user's own prompts as they are typed; a worker's instruction
                // arrives here, from the thread that is about to work on it.
                if id != Swarm::ROOT {
                    let _ = ui.send(UiEvent::Instruction { id, text });
                }
            }
            Event::Agent { id, kind, text } => {
                let (agents, busy) = {
                    let mut tree = lock(&swarm);
                    tree.finish_turn(id, kind, &text);
                    tree.deliver(id, kind, &text);
                    (tree.snapshot(), tree.busy())
                };
                let shown = if id == Swarm::ROOT {
                    match kind {
                        AgentEvent::Finished => UiEvent::Answer(text),
                        AgentEvent::Failed => UiEvent::Error(text),
                        _ => UiEvent::Notice(format!("agent {id} {}", kind.verb())),
                    }
                } else {
                    UiEvent::Agent { id, kind, text }
                };
                let _ = ui.send(shown);
                let _ = ui.send(UiEvent::Agents(agents));
                let _ = ui.send(UiEvent::Busy(busy));
                if draining && !busy {
                    break;
                }
            }
            Event::User(input) => {
                draining = false;
                // The guard must not outlive `tell_root`: a `match` scrutinee's temporary lives for
                // the whole match, and the snapshot below takes the same lock again.
                let told = lock(&swarm).tell_root(input);
                match told {
                    Ok(_) => {
                        let _ = ui.send(UiEvent::Agents(lock(&swarm).snapshot()));
                        let _ = ui.send(UiEvent::Busy(true));
                    }
                    Err(e) => {
                        let _ = ui.send(UiEvent::Error(e));
                    }
                }
            }
        }
    }
    // Quitting must not leave an agent's `cargo build` running without its owner, nor an MCP server.
    tools::kill_running();
    mcp.kill_all();
    let _ = ui.send(UiEvent::Done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Theme};
    use crate::llm::testing::{Scripted, text};
    use std::collections::HashMap;
    use std::fs;
    use std::sync::mpsc::{Receiver, channel};
    use std::thread;
    use std::time::{Duration, Instant};

    /// A compaction reaches the user as a notice, with the recovery band named when it applies.
    #[test]
    fn a_compaction_reaches_the_user_as_a_notice() {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: None,
            pricing: HashMap::new(),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let frontend = input.clone();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(Vec::new())))),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg,
                logs.path().to_path_buf(),
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, HashMap::new())),
                None,
            )
        });
        frontend
            .send(Event::Compaction {
                id: 1,
                notice: CompactionNotice::Compacted { before: 900, after: 100, over_band: false },
            })
            .unwrap();
        assert_eq!(next_notice(&ui_rx), format!("compacted {}: 900 → 100 tokens", names::label(1, None)));
        frontend
            .send(Event::Compaction {
                id: 2,
                notice: CompactionNotice::Compacted { before: 900, after: 800, over_band: true },
            })
            .unwrap();
        assert_eq!(
            next_notice(&ui_rx),
            format!("compacted {}: 900 → 800 tokens; still above the recovery band", names::label(2, None))
        );
        frontend.send(Event::Eof).unwrap();
        handle.join().unwrap().unwrap();
    }

    /// The whole wire: a user line reaches the root, its answer comes back, and the frontend quits.
    #[test]
    fn a_users_line_reaches_the_root_and_its_answer_comes_back() {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: None,
            pricing: HashMap::new(),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let frontend = input.clone();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(vec![text("hello back")])))),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg,
                logs.path().to_path_buf(),
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, HashMap::new())),
                None,
            )
        });
        frontend.send(Event::User("hello".into())).unwrap();
        let answer = next_answer(&ui_rx);
        assert_eq!(answer, "hello back");
        // End of input: the run must end by itself once no agent is busy, never hang on a closed stdin.
        frontend.send(Event::Eof).unwrap();
        let mut seen: Vec<String> = Vec::new();
        let done = loop {
            match ui_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(UiEvent::Done) => break UiEvent::Done,
                Ok(event) => seen.push(format!("{event:?}")),
                Err(e) => panic!("the run never ended on end of input ({e}); it saw {seen:?}"),
            }
        };
        assert_eq!(done, UiEvent::Done);
        handle.join().unwrap().unwrap();
    }

    /// A picture pasted with a prompt reaches the model on the user message, not as text.
    #[test]
    fn a_pasted_picture_reaches_the_model() {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: None,
            pricing: HashMap::new(),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let recording =
            Arc::new(crate::llm::testing::Recording::new(Arc::new(Scripted::new(vec![text("a screenshot")]))));
        let llm: Arc<dyn crate::llm::Llm> = recording.clone();
        let frontend = input.clone();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(llm)),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg,
                logs.path().to_path_buf(),
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, HashMap::new())),
                None,
            )
        });
        let png = root.path().join("shot.png");
        frontend
            .send(Event::User(crate::swarm::Input { text: "what is this?".into(), images: vec![png.clone()] }))
            .unwrap();
        assert_eq!(next_answer(&ui_rx), "a screenshot");
        let requests = recording.requests.lock().unwrap().clone();
        let last = requests[0].last().unwrap();
        assert_eq!(last.role, "user");
        assert_eq!(last.images, [png]);
        frontend.send(Event::Eof).unwrap();
        handle.join().unwrap().unwrap();
    }

    /// A core whose root cannot start reports it, then ends the frontend like any other quit.
    #[test]
    fn a_failed_start_still_ends_the_frontend() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: None,
            pricing: HashMap::new(),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        // No log directory: the root agent's log file cannot be opened.
        let logs = root.path().join("missing");
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(vec![text("unused")])))),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg,
                logs,
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, HashMap::new())),
                None,
            )
        });
        let mut seen: Vec<String> = Vec::new();
        loop {
            match ui_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(UiEvent::Done) => break,
                Ok(event) => seen.push(format!("{event:?}")),
                Err(e) => panic!("a failed start never ended the frontend ({e}); it saw {seen:?}"),
            }
        }
        assert!(seen.iter().any(|event| event.starts_with("Error(")), "{seen:?}");
        assert!(handle.join().unwrap().is_err());
    }

    /// Drains the frontend until one event's debug form contains `needle`, and returns what it saw.
    fn until(ui: &Receiver<UiEvent>, needle: &str) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(event) = ui.recv_timeout(Duration::from_millis(100)) {
                let line = format!("{event:?}");
                seen.push(line.clone());
                if line.contains(needle) {
                    return seen;
                }
            }
        }
        panic!("no event contained {needle:?}; it saw {seen:?}");
    }

    /// `/pipeline` reaches the catalogue and the tree without a model call: list, start, stop, and
    /// the root still answers afterwards.
    #[test]
    fn the_pipeline_slash_command_lists_starts_and_stops() {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let dir = root.path().join(pipelines::ROOT);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("fixture.rhai"), "//! requires:\nreport(\"done\");\n").unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: None,
            pricing: HashMap::new(),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let catalogue = Arc::new(pipelines::Catalogue::load(root.path()));
        let frontend = input.clone();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(vec![text("hello back")])))),
                Arc::new(profiles::Catalogue::default()),
                catalogue,
                &cfg,
                logs.path().to_path_buf(),
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, HashMap::new())),
                None,
            )
        });
        frontend.send(Event::from_line("/pipeline list").unwrap()).unwrap();
        assert!(until(&ui_rx, "Notice(\"fixture\")").iter().any(|line| line.contains("fixture")));
        frontend.send(Event::from_line("/pipeline fixture").unwrap()).unwrap();
        // The root is agent 1, so the first pipeline it starts is agent 2.
        assert!(until(&ui_rx, "started as").iter().any(|line| line.contains("(2)")));
        frontend.send(Event::from_line("/pipeline stop 2").unwrap()).unwrap();
        assert!(until(&ui_rx, "stopped").iter().any(|line| line.contains("stopped")));
        frontend.send(Event::User("hello".into())).unwrap();
        assert!(until(&ui_rx, "hello back").iter().any(|line| line.contains("hello back")));
        frontend.send(Event::Eof).unwrap();
        assert!(until(&ui_rx, "Done").iter().any(|line| line.contains("Done")));
        handle.join().unwrap().unwrap();
    }

    /// One session answers a line and exits; a second one resumes it: the roster comes back and the
    /// root answers a new line with the replayed history behind it.
    #[test]
    fn a_session_resumes_the_tree_of_the_one_before() {
        let root = tempfile::tempdir().unwrap();
        let first_logs = tempfile::tempdir().unwrap();
        let second_logs = tempfile::tempdir().unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: None,
            pricing: HashMap::new(),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            mcp_project: config::McpProject::On,
            approvals: crate::approvals::Tier::Ask,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let frontend = input.clone();
        let first_dir = first_logs.path().to_path_buf();
        let cfg_first = cfg.clone();
        let home = root.path().to_path_buf();
        let home_first = home.clone();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(vec![text("first answer")])))),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg_first,
                first_dir,
                input,
                input_rx,
                ui,
                Arc::new(mcp::Registry::load(&home_first)),
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, HashMap::new())),
                None,
            )
        });
        frontend.send(Event::User("remember this".into())).unwrap();
        assert_eq!(next_answer(&ui_rx), "first answer");
        frontend.send(Event::Eof).unwrap();
        assert!(until(&ui_rx, "Done").iter().any(|line| line.contains("Done")));
        handle.join().unwrap().unwrap();

        // The first session left a tree record and the root's committed history behind.
        let source = crate::session_store::ResumeSource { id: "first".into(), dir: first_logs.path().to_path_buf() };
        assert!(first_logs.path().join("session.jsonl").is_file());
        assert!(first_logs.path().join("checkpoints/agent-1/1.history.json").is_file());

        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let frontend = input.clone();
        let second_dir = second_logs.path().to_path_buf();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(vec![text("second answer")])))),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg,
                second_dir,
                input,
                input_rx,
                ui,
                Arc::new(mcp::Registry::load(&home)),
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, HashMap::new())),
                Some(Restore::Resume(source)),
            )
        });
        // The resume notice names the agents that came back.
        assert!(until(&ui_rx, "resumed").iter().any(|line| line.contains("1 agent(s) back")));
        frontend.send(Event::User("what did I say?".into())).unwrap();
        assert_eq!(next_answer(&ui_rx), "second answer");
        frontend.send(Event::Eof).unwrap();
        assert!(until(&ui_rx, "Done").iter().any(|line| line.contains("Done")));
        handle.join().unwrap().unwrap();
    }

    /// The first notice about a compaction: startup notices come before it.
    fn next_notice(ui: &Receiver<UiEvent>) -> String {
        let mut seen: Vec<String> = Vec::new();
        loop {
            match ui.recv_timeout(Duration::from_secs(5)) {
                Ok(UiEvent::Notice(text)) if text.starts_with("compacted ") => return text,
                Ok(event) => seen.push(format!("{event:?}")),
                Err(e) => panic!("no notice reached the frontend ({e}); it saw {seen:?}"),
            }
        }
    }

    /// The first answer the frontend prints; on a timeout it names everything else that arrived.
    fn next_answer(ui: &Receiver<UiEvent>) -> String {
        let mut seen: Vec<String> = Vec::new();
        loop {
            match ui.recv_timeout(Duration::from_secs(5)) {
                Ok(UiEvent::Answer(text)) => return text,
                Ok(event) => seen.push(format!("{event:?}")),
                Err(e) => panic!("no answer reached the frontend ({e}); it saw {seen:?}"),
            }
        }
    }

    /// An approval request reaches the frontend as a `UiEvent`, and the answer through the
    /// frontend's input lets the gated call run and the turn finish.
    #[test]
    fn an_approval_request_reaches_the_frontend_and_the_answer_runs_the_tool() {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), "").unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: None,
            pricing: std::collections::HashMap::new(),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let frontend = input.clone();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(vec![
                    crate::llm::testing::call("c1", crate::tools::BASH, &serde_json::json!({"command": "ls"})),
                    text("done"),
                ])))),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg,
                logs.path().to_path_buf(),
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(bounds::Bounds::new(None, None, std::collections::HashMap::new())),
                None,
            )
        });
        frontend.send(Event::User("run".into())).unwrap();
        let mut request = None;
        let mut seen: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while request.is_none() {
            assert!(Instant::now() < deadline, "no approval request reached the frontend; it saw {seen:?}");
            match ui_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(UiEvent::ApprovalRequest { request: id, .. }) => request = Some(id),
                Ok(event) => seen.push(format!("{event:?}")),
                Err(e) => panic!("the frontend stalled ({e}); it saw {seen:?}"),
            }
        }
        frontend.send(Event::Resolve { request: request.unwrap(), choice: crate::approvals::Choice::Once }).unwrap();
        assert_eq!(next_answer(&ui_rx), "done");
        frontend.send(Event::Eof).unwrap();
        handle.join().unwrap().unwrap();
    }

    /// A charge reaches the frontend twice: once as the meter, once as the warn ceiling's notice,
    /// and the notice never repeats.
    #[test]
    fn a_charge_reaches_the_meter_and_crosses_the_warn_ceiling_once() {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let cfg = config::Config {
            roots: vec![root.path().to_path_buf()],
            base_url: String::new(),
            model: "m".into(),
            theme: Theme::Auto,
            max_depth: 3,
            max_time: None,
            max_cost: None,
            warn_cost: Some(1.0),
            pricing: HashMap::from([("m".to_string(), crate::bounds::Price { input: 1.0, output: 0.0 })]),
            dir: root.path().to_path_buf(),
            rule_dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            compaction: crate::config::CompactionSettings::default(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let frontend = input.clone();
        // The meter is where a charge lands in this build; the model is scripted and reports none,
        // so the test charges the same meter a real call would.
        let meter = Arc::new(bounds::Bounds::new(None, None, cfg.pricing.clone()).with_warn_cost(cfg.warn_cost));
        let charged = Arc::clone(&meter);
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(Vec::new())))),
                Arc::new(profiles::Catalogue::default()),
                Arc::new(pipelines::Catalogue::default()),
                &cfg,
                logs.path().to_path_buf(),
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(Gate::new(crate::approvals::Tier::Ask, true)),
                Arc::new(AtomicBool::new(false)),
                meter,
                None,
            )
        });
        charged.account(1, "m", (1_000_000, 0));
        assert_eq!(charged.spent(), 1.0);
        let mut saw_meter = None;
        let mut warn: Option<String> = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while saw_meter.is_none() || warn.is_none() {
            assert!(Instant::now() < deadline, "the meter never reached the frontend");
            match ui_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(UiEvent::Meter { cost, tokens, agents }) => saw_meter = Some((cost, tokens, agents)),
                Ok(UiEvent::Notice(text)) if text.contains("warn_cost") => warn = Some(text),
                Ok(_) | Err(_) => {}
            }
        }
        let (cost, tokens, agents) = saw_meter.expect("the meter");
        assert_eq!(cost, 1.0);
        assert_eq!(tokens, 1_000_000);
        assert_eq!(agents.len(), 1);
        let warn = warn.expect("the warn notice");
        assert!(warn.contains("$1.00"), "{warn}");
        // A second crossing of the same ceiling says nothing more.
        charged.account(1, "m", (1_000_000, 0));
        frontend.send(Event::Eof).unwrap();
        handle.join().unwrap().unwrap();
    }
}
