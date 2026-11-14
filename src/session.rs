use crate::agent;
use crate::config;
use crate::event::UiEvent;
use crate::fleet::{self, Event, Fleet};
use crate::llm::{Llm, Message, StreamLlm, StreamStep, ToolCall};
use crate::prompt;
use crate::tools;
use crate::version;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// Runs one overseer turn per event until the frontend quits or disconnects.
///
/// Blocking by design: the model client and every worker are blocking, so a frontend keeps this on its own thread.
/// Each channel is moved in, because whoever ends the thread ends the connection.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run(
    llm: Arc<dyn Llm>,
    cfg: &config::Config,
    log_dir: PathBuf,
    input: Sender<Event>,
    events: Receiver<Event>,
    ui: Sender<UiEvent>,
) {
    let debug = std::env::var_os(config::DEBUG).is_some();
    let _ = ui.send(UiEvent::Notice(format!("{} · worker logs in {}", version::VERSION, log_dir.display())));
    let mut fleet = Fleet::new(Arc::clone(&llm), cfg.roots.clone(), cfg.home.clone(), log_dir, input);
    // The overseer's calls stream into the transcript; the fleet keeps the raw client so worker
    // output does not also arrive as overseer text.
    let stream_ui = ui.clone();
    let overseer: Arc<dyn Llm> = Arc::new(StreamLlm::new(
        Arc::clone(&llm),
        Arc::new(move |step| {
            let event = match step {
                StreamStep::Start => UiEvent::TurnBoundary,
                StreamStep::Text(delta) => UiEvent::Delta(delta.to_string()),
            };
            let _ = stream_ui.send(event);
        }),
    ));
    let mut history = vec![Message::system(prompt::overseer_prompt(&cfg.roots))];
    let tool_defs = prompt::overseer_tool_definitions();
    let mut draining = false;
    let mut tool_id = 0_usize;
    while let Ok(event) = events.recv() {
        let text = match event {
            Event::Exit => break,
            Event::Eof => {
                draining = true;
                if fleet.busy() {
                    let _ = ui.send(UiEvent::Notice("draining: quits when no worker is busy".into()));
                    continue;
                }
                break;
            }
            Event::Resume(arg) => {
                let line = fleet.resume(&arg);
                let _ = ui.send(UiEvent::Notice(line));
                let _ = ui.send(UiEvent::Workers(fleet.snapshot()));
                continue;
            }
            Event::Progress { id, progress } => {
                let _ = ui.send(UiEvent::Progress { id, progress });
                continue;
            }
            Event::Delta { id, text } => {
                let _ = ui.send(UiEvent::WorkerDelta { id, text });
                continue;
            }
            Event::Worker { id, kind, text } => {
                fleet.record(id, kind);
                let _ = ui.send(UiEvent::Workers(fleet.snapshot()));
                let _ = ui.send(UiEvent::Worker { id, kind, text: text.clone() });
                format!("[worker {id} {}] {text}", kind.verb())
            }
            // A prompt typed after ctrl+D cancels the drain: the user changed their mind.
            Event::User(text) => {
                draining = false;
                text
            }
        };
        let _ = ui.send(UiEvent::Busy(true));
        {
            let mut execute = |call: &ToolCall| {
                let id = tool_id;
                tool_id += 1;
                let summary = fleet::describe(&call.function.name, &call.function.arguments);
                let started = Instant::now();
                let _ = ui.send(UiEvent::ToolStarted { id, tool: call.function.name.clone(), summary });
                let output = fleet.execute(call);
                let secs = started.elapsed().as_secs_f64();
                let preview = tools::preview(&call.function.name, &output);
                let _ = ui.send(UiEvent::ToolFinished { id, preview, secs });
                if debug {
                    let _ = ui.send(UiEvent::Debug(output.clone()));
                }
                let _ = ui.send(UiEvent::Workers(fleet.snapshot()));
                output
            };
            let mut turn = agent::Turn { llm: overseer.as_ref(), tool_defs: &tool_defs, execute: &mut execute };
            match turn.run(&mut history, &text) {
                Ok(answer) if !answer.is_empty() => {
                    let _ = ui.send(UiEvent::Answer(answer));
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = ui.send(UiEvent::Error(format!("{e:#}")));
                }
            }
        }
        let _ = ui.send(UiEvent::Busy(false));
        let _ = ui.send(UiEvent::Workers(fleet.snapshot()));
        if draining && !fleet.busy() {
            break;
        }
    }
    // Quitting must not leave a worker's `cargo build` running without its owner.
    tools::kill_running();
    let _ = ui.send(UiEvent::Done);
}
