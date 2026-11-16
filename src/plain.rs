use crate::event::UiEvent;
use crate::names;
use crate::swarm::{AgentView, Event, Progress};
use serde_json::json;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

/// How the line-mode frontend was asked to run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Args {
    /// A prompt given with `-p`/`--print`: one turn, then exit.
    pub(crate) print: Option<String>,
    /// `--mode json`: every event becomes one compact JSON line on stdout.
    pub(crate) json: bool,
}

/// Line mode: stdin lines become events, `UiEvent`s become plain lines.
///
/// Used when no terminal is attached, so a pipe or a script still drives the overseer.
pub(crate) fn run(ui: Receiver<UiEvent>, input: Sender<Event>, args: &Args) -> anyhow::Result<()> {
    if let Some(prompt) = &args.print {
        // The prompt reaches the model verbatim, never the line parser: `exit` is a prompt, not a command.
        let _ = input.send(Event::User(prompt.clone()));
        let _ = input.send(Event::Eof);
    } else {
        spawn_stdin_reader(input);
    }
pub(crate) fn run(ui: Receiver<UiEvent>, input: Sender<Event>) {
    spawn_stdin_reader(input);
    // The roster, for the one thing line mode needs from it: the name of an agent it reports on.
    let mut agents: Vec<AgentView> = Vec::new();
    // Which tool each running call was: keyed by (agent id, call number), filled on start.
    let mut tools: HashMap<(usize, usize), String> = HashMap::new();
    let mut answered = false;
    for event in ui {
        match event {
            UiEvent::Answer(text) => {
                answered = true;
                if args.json {
                    out(&json!({"type": "answer", "text": text}).to_string());
                } else {
                    out(&text);
            UiEvent::Notice(line) => err(&format!("overseer: {line}")),
            UiEvent::Error(line) => err(&format!("error: {line}")),
            UiEvent::Debug(line) => err(&line),
            UiEvent::Answer(text) => out(&text),
            UiEvent::Agent { id, kind, text } => {
                let mut stderr = io::stderr().lock();
                let mut lines = text.lines();
                // The id comes first, as in the transcript; the name follows it because a person reads this too.
                let _ = writeln!(
                    stderr,
                    "agent {id} · {} {}: {}",
                    name(&agents, id),
                    kind.verb(),
                    lines.next().unwrap_or_default()
                );
                for line in lines {
                    let _ = writeln!(stderr, "  {line}");
                }
            }
            UiEvent::Out { id, text } => out(&format!("[agent {id} · {} out]\n{text}", name(&agents, id))),
            UiEvent::Progress { id, progress } => err(&format!("[{}] {}", label(&agents, id), describe(&progress))),
            // Line mode writes whole lines: what streams, and where the screen breaks it, is the screen's.
            UiEvent::AgentDelta { .. }
            | UiEvent::AgentThought { .. }
            | UiEvent::Boundary { .. }
            | UiEvent::Instruction { .. }
            | UiEvent::Busy(_) => {}
            UiEvent::Agents(roster) => agents = roster,
            UiEvent::Done => break,
            event => {
                let line = if args.json {
                    match json_event(&mut agents, &mut tools, event) {
                        Some(value) => value.to_string(),
                        None => continue,
                    }
                } else {
                    plain_event(&mut agents, event);
                    continue;
                };
                out(&line);
            }
        }
    }
    if args.print.is_some() && !answered {
        anyhow::bail!("the root did not answer");
    }
    Ok(())
}

/// What one `UiEvent` prints in plain line mode; `Answer` and `Done` are the run loop's.
fn plain_event(agents: &mut Vec<AgentView>, event: UiEvent) {
    match event {
        UiEvent::Notice(line) => err(&format!("overseer: {line}")),
        UiEvent::Error(line) => err(&format!("error: {line}")),
        UiEvent::Debug(line) => err(&line),
        UiEvent::Agent { id, kind, text } => {
            let mut stderr = io::stderr().lock();
            let mut lines = text.lines();
            // The id comes first, as in the transcript; the name follows it because a person reads this too.
            let _ = writeln!(
                stderr,
                "agent {id} · {} {}: {}",
                name(agents, id),
                kind.verb(),
                lines.next().unwrap_or_default()
            );
            for line in lines {
                let _ = writeln!(stderr, "  {line}");
            }
        }
        UiEvent::Out { id, text } => out(&format!("[agent {} · {} out]\n{text}", id, name(agents, id))),
        UiEvent::Progress { id, progress } => {
            err(&format!("[{}] {}", label(agents, id), describe(&progress)));
        }
        // Line mode writes whole lines: what streams, and where the screen breaks it, is the screen's.
        UiEvent::AgentDelta { .. }
        | UiEvent::AgentThought { .. }
        | UiEvent::Boundary { .. }
        | UiEvent::Instruction { .. }
        | UiEvent::Busy(_) => {}
        UiEvent::Agents(roster) => *agents = roster,
        UiEvent::Answer(_) | UiEvent::Done => unreachable!("handled by run"),
    }
}

/// What one `UiEvent` prints in JSON mode, as one object; `None` prints nothing.
fn json_event(
    agents: &mut Vec<AgentView>,
    tools: &mut HashMap<(usize, usize), String>,
    event: UiEvent,
) -> Option<serde_json::Value> {
    Some(match event {
        UiEvent::Notice(text) => json!({"type": "notice", "text": text}),
        UiEvent::Error(text) => json!({"type": "error", "text": text}),
        UiEvent::Debug(text) => json!({"type": "debug", "text": text}),
        UiEvent::Answer(_) | UiEvent::Done => unreachable!("handled by run"),
        UiEvent::Agent { id, kind, text } => {
            json!({"type": "agent", "id": id, "name": name(agents, id), "kind": kind.verb(), "text": text})
        }
        UiEvent::Out { id, text } => json!({"type": "out", "id": id, "name": name(agents, id), "text": text}),
        UiEvent::Progress { id, progress } => match progress {
            Progress::Started { call, tool, summary } => {
                tools.insert((id, call), tool.clone());
                json!({"type": "tool", "id": id, "call": call, "phase": "start", "name": tool, "summary": summary})
            }
            Progress::Finished { call, secs, preview } => {
                let name = tools.remove(&(id, call)).unwrap_or_default();
                json!({
                    "type": "tool", "id": id, "call": call, "phase": "done", "name": name,
                    "secs": secs, "status": preview.status, "head": preview.head,
                    "more": preview.more, "failed": preview.failed,
                })
            }
        },
        UiEvent::Agents(roster) => {
            *agents = roster;
            return None;
        }
        // Line mode writes whole lines: what streams, and where the screen breaks it, is the screen's.
        UiEvent::AgentDelta { .. }
        | UiEvent::AgentThought { .. }
        | UiEvent::Boundary { .. }
        | UiEvent::Instruction { .. }
        | UiEvent::Busy(_) => {
            return None;
        }
    })
}

/// An agent's name alone, for a line that already carries its id.
fn name(agents: &[AgentView], id: usize) -> String {
    agents.iter().find(|agent| agent.id == id).map_or_else(|| names::name(id, None), |agent| agent.name.clone())
}

/// How an agent reads in a prefixed line: its name and the id it answers to.
fn label(agents: &[AgentView], id: usize) -> String {
    agents.iter().find(|agent| agent.id == id).map_or_else(|| names::label(id, None), |agent| agent.label.clone())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::AgentEvent;
    use crate::tools::Preview;
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn preview() -> Preview {
        Preview { status: "exit 0".into(), head: "done".into(), more: 1, failed: false }
    }

    fn roster() -> Vec<AgentView> {
        vec![AgentView {
            id: 2,
            name: "brave-otter".into(),
            label: "brave-otter (2)".into(),
            parent: None,
            depth: 1,
            dir: PathBuf::from("."),
            task: String::new(),
            profile: String::new(),
            supervisor: false,
            state: crate::swarm::State::Idle,
            read_only: false,
            busy_for: None,
        }]
    }

    #[test]
    fn json_event_maps_every_emitted_variant() {
        let mut agents = roster();
        let mut tools = HashMap::new();

        assert_eq!(
            json_event(&mut agents, &mut tools, UiEvent::Notice("n".into())).unwrap(),
            json!({"type": "notice", "text": "n"})
        );
        assert_eq!(
            json_event(&mut agents, &mut tools, UiEvent::Error("e".into())).unwrap(),
            json!({"type": "error", "text": "e"})
        );
        assert_eq!(
            json_event(&mut agents, &mut tools, UiEvent::Debug("d".into())).unwrap(),
            json!({"type": "debug", "text": "d"})
        );
        assert_eq!(
            json_event(
                &mut agents,
                &mut tools,
                UiEvent::Agent { id: 2, kind: AgentEvent::Finished, text: "done".into() }
            )
            .unwrap(),
            json!({"type": "agent", "id": 2, "name": "brave-otter", "kind": "finished", "text": "done"})
        );
        assert_eq!(
            json_event(&mut agents, &mut tools, UiEvent::Out { id: 2, text: "o".into() }).unwrap(),
            json!({"type": "out", "id": 2, "name": "brave-otter", "text": "o"})
        );
        assert_eq!(
            json_event(
                &mut agents,
                &mut tools,
                UiEvent::Progress {
                    id: 2,
                    progress: Progress::Started { call: 1, tool: "bash".into(), summary: "ls".into() }
                }
            )
            .unwrap(),
            json!({"type": "tool", "id": 2, "call": 1, "phase": "start", "name": "bash", "summary": "ls"})
        );
        assert_eq!(
            json_event(
                &mut agents,
                &mut tools,
                UiEvent::Progress { id: 2, progress: Progress::Finished { call: 1, secs: 1.5, preview: preview() } }
            )
            .unwrap(),
            json!({
                "type": "tool", "id": 2, "call": 1, "phase": "done", "name": "bash",
                "secs": 1.5, "status": "exit 0", "head": "done", "more": 1, "failed": false,
            })
        );
    }

    #[test]
    fn a_finished_without_a_started_name_prints_an_empty_name() {
        let mut agents = roster();
        let mut tools = HashMap::new();
        let value = json_event(
            &mut agents,
            &mut tools,
            UiEvent::Progress { id: 2, progress: Progress::Finished { call: 7, secs: 0.0, preview: preview() } },
        )
        .unwrap();
        assert_eq!(value["name"], "");
    }

    #[test]
    fn the_roster_and_streaming_events_print_nothing() {
        let mut agents = roster();
        let mut tools = HashMap::new();
        for event in [
            UiEvent::Agents(roster()),
            UiEvent::AgentDelta { id: 2, text: "x".into() },
            UiEvent::AgentThought { id: 2, text: "t".into() },
            UiEvent::Boundary { id: 2 },
            UiEvent::Instruction { id: 2, text: "i".into() },
            UiEvent::Busy(true),
        ] {
            assert!(json_event(&mut agents, &mut tools, event).is_none());
        }
    }

    #[test]
    fn print_mode_ends_ok_when_the_root_answered() {
        let (ui_tx, ui_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        ui_tx.send(UiEvent::Answer("hello".into())).unwrap();
        ui_tx.send(UiEvent::Done).unwrap();
        drop(ui_tx);
        run(ui_rx, input_tx, &Args { print: Some("hi".into()), json: false }).unwrap();
        // The prompt went to the core verbatim, followed by the idle quit.
        assert_eq!(input_rx.recv().unwrap(), Event::User("hi".into()));
        assert_eq!(input_rx.recv().unwrap(), Event::Eof);
    }

    #[test]
    fn print_mode_ends_in_error_when_the_root_never_answered() {
        let (ui_tx, ui_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        ui_tx.send(UiEvent::Done).unwrap();
        drop(ui_tx);
        let error = run(ui_rx, input_tx, &Args { print: Some("hi".into()), json: false }).unwrap_err();
        assert_eq!(error.to_string(), "the root did not answer");
        assert_eq!(input_rx.recv().unwrap(), Event::User("hi".into()));
    }

    #[test]
    fn line_mode_still_ends_ok_without_an_answer() {
        let (ui_tx, ui_rx) = mpsc::channel();
        let (input_tx, _) = mpsc::channel();
        ui_tx.send(UiEvent::Done).unwrap();
        run(ui_rx, input_tx, &Args { print: None, json: false }).unwrap();
    }

    #[test]
    fn print_and_json_together_answer_as_one_line_and_end_ok() {
        let (ui_tx, ui_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        ui_tx.send(UiEvent::Agents(roster())).unwrap();
        ui_tx.send(UiEvent::Answer("hello".into())).unwrap();
        ui_tx.send(UiEvent::Done).unwrap();
        drop(ui_tx);
        run(ui_rx, input_tx, &Args { print: Some("hi".into()), json: true }).unwrap();
        assert_eq!(input_rx.recv().unwrap(), Event::User("hi".into()));
    }
}
