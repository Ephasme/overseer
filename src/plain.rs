use crate::event::UiEvent;
use crate::swarm::{Event, Progress};
use std::io::{self, BufRead, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

/// Line mode: stdin lines become events, `UiEvent`s become plain lines.
///
/// Used when no terminal is attached, so a pipe or a script still drives the overseer.
pub(crate) fn run(ui: Receiver<UiEvent>, input: Sender<Event>) {
    spawn_stdin_reader(input);
    for event in ui {
        match event {
            UiEvent::Notice(line) => err(&format!("overseer: {line}")),
            UiEvent::Error(line) => err(&format!("error: {line}")),
            UiEvent::Debug(line) => err(&line),
            UiEvent::Answer(text) => out(&text),
            UiEvent::Agent { id, kind, text } => {
                let mut stderr = io::stderr().lock();
                let mut lines = text.lines();
                let _ = writeln!(stderr, "agent {id} {}: {}", kind.verb(), lines.next().unwrap_or_default());
                for line in lines {
                    let _ = writeln!(stderr, "  {line}");
                }
            }
            UiEvent::Progress { id, progress } => err(&format!("[agent {id}] {}", describe(&progress))),
            UiEvent::AgentDelta { .. } | UiEvent::TurnBoundary | UiEvent::Agents(_) | UiEvent::Busy(_) => {}
            UiEvent::Done => break,
        }
    }
}

/// A pipe may close before the overseer is done writing: `EPIPE` ends the output, never a panic.
fn out(line: &str) {
    let _ = writeln!(io::stdout().lock(), "{line}");
}

fn err(line: &str) {
    let _ = writeln!(io::stderr().lock(), "{line}");
}

fn describe(progress: &Progress) -> String {
    match progress {
        Progress::Started { tool, summary, .. } => format!("{tool} {summary}"),
        Progress::Finished { secs, preview, .. } => {
            let status = if preview.status.is_empty() { String::new() } else { format!(" {}", preview.status) };
            let head = preview.head.lines().next().unwrap_or_default();
            let more = if preview.more == 0 { String::new() } else { format!(" (+{} more lines)", preview.more) };
            if preview.failed {
                format!("failed{status}: {head}{more}")
            } else if head.is_empty() {
                format!("ok{status} in {secs:.1}s{more}")
            } else {
                format!("ok{status} in {secs:.1}s: {head}{more}")
            }
        }
    }
}

/// Reads stdin into `input` until EOF, mapping each line to an `Event`.
fn spawn_stdin_reader(input: Sender<Event>) {
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    err(&format!("overseer: stdin: {error}"));
                    break;
                }
            };
            let Some(event) = Event::from_line(line.trim()) else { continue };
            let exit = event == Event::Exit;
            if input.send(event).is_err() || exit {
                return;
            }
        }
        let _ = input.send(Event::Eof);
    });
}
