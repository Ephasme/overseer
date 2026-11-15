use crate::config;
use crate::event::UiEvent;
use crate::llm::Models;
use crate::mcp;
use crate::profiles;
use crate::swarm::{AgentEvent, Event, Swarm, lock};
use crate::tools;
use crate::url;
use crate::version;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the loop looks at the shutdown flag, so a frontend that returned cannot be waited on.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Routes events between the frontends and the tree; it never calls a model itself.
///
/// Blocking by design: the model client and every agent are blocking, so a frontend keeps this on its own thread.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
pub(crate) fn run(
    models: Arc<dyn Models>,
    profiles: Arc<profiles::Catalogue>,
    cfg: &config::Config,
    log_dir: PathBuf,
    input: Sender<Event>,
    events: Receiver<Event>,
    ui: Sender<UiEvent>,
    mcp: Arc<mcp::Registry>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let _ = ui.send(UiEvent::Notice(format!("{} · agent logs in {}", version::VERSION, log_dir.display())));
    let swarm =
        Arc::new(Mutex::new(Swarm::new(models, profiles, Arc::new(cfg.clone()), log_dir, input, Arc::clone(&mcp))));
    if let Err(e) = Swarm::start_root(&swarm, cfg.dir.clone()) {
        let _ = ui.send(UiEvent::Error(e.clone()));
        tools::kill_running();
        mcp.kill_all();
        let _ = ui.send(UiEvent::Done);
        return Err(anyhow::Error::msg(e));
    }
    let _ = ui.send(UiEvent::Agents(lock(&swarm).snapshot()));
    // Set when stdin closes: the run ends at the first moment no agent is busy.
    let mut draining = false;
    while !shutdown.load(Ordering::Relaxed) {
        let event = match events.recv_timeout(SHUTDOWN_POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
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
            Event::User(text) => {
                draining = false;
                // The guard must not outlive `tell_root`: a `match` scrutinee's temporary lives for
                // the whole match, and the snapshot below takes the same lock again.
                let told = lock(&swarm).tell_root(&text);
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
    use std::sync::mpsc::{Receiver, channel};
    use std::thread;
    use std::time::Duration;

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
            dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
        };
        let (input, input_rx) = channel();
        let (ui, ui_rx) = channel();
        let mcp = Arc::new(mcp::Registry::load(root.path()));
        let frontend = input.clone();
        let handle = thread::spawn(move || {
            run(
                Arc::new(crate::llm::testing::Fixed(Arc::new(Scripted::new(vec![text("hello back")])))),
                Arc::new(profiles::Catalogue::default()),
                &cfg,
                logs.path().to_path_buf(),
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(AtomicBool::new(false)),
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
            dir: root.path().to_path_buf(),
            home: root.path().to_path_buf(),
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
                &cfg,
                logs,
                input,
                input_rx,
                ui,
                mcp,
                Arc::new(AtomicBool::new(false)),
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
}
