use crate::event::UiEvent;
use crate::swarm::{Event, Progress};
use std::io::{self, BufRead};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

/// Line mode: stdin lines become events, `UiEvent`s become plain lines.
///
/// Used when no terminal is attached, so a pipe or a script still drives the overseer.
pub(crate) fn run(ui: Receiver<UiEvent>, input: Sender<Event>) {
    spawn_stdin_reader(input);
    for event in ui {
        match event {
            UiEvent::Notice(line) => eprintln!("overseer: {line}"),
            UiEvent::Error(line) => eprintln!("error: {line}"),
            UiEvent::Debug(line) => eprintln!("{line}"),
            UiEvent::Answer(text) => println!("{text}"),
            UiEvent::Agent { id, kind, text } => {
                let line = text.lines().next().unwrap_or_default();
                eprintln!("agent {id} {}: {line}", kind.verb());
            }
            UiEvent::Progress { id, progress } => eprintln!("[agent {id}] {}", describe(&progress)),
            UiEvent::AgentDelta { .. } | UiEvent::TurnBoundary | UiEvent::Agents(_) | UiEvent::Busy(_) => {}
            UiEvent::Done => break,
        }
    }
}

fn describe(progress: &Progress) -> String {
    match progress {
        Progress::Started { tool, summary, .. } => format!("{tool} {summary}"),
        Progress::Finished { secs, preview, .. } => {
            let status = if preview.status.is_empty() { String::new() } else { format!(" {}", preview.status) };
            let head = preview.head.lines().next().unwrap_or_default();
            if preview.failed { format!("failed{status}: {head}") } else { format!("ok{status} in {secs:.1}s") }
        }
    }
}

/// Reads stdin into `input` until EOF, mapping each line to an `Event`.
fn spawn_stdin_reader(input: Sender<Event>) {
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Some(event) = Event::from_line(line.trim()) else { continue };
            let exit = event == Event::Exit;
            if input.send(event).is_err() || exit {
                return;
            }
        }
        let _ = input.send(Event::Eof);
    });
}
