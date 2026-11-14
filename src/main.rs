mod agent;
mod config;
mod fleet;
mod llm;
mod prompt;
mod repos;
mod secrets;
mod tools;

use anyhow::{Context, Result};
use fleet::{Event, Fleet};
use llm::{Llm, Message, ToolCall};
use serde_json::Value;
use std::io::{self, BufRead};
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, thread};

fn main() -> Result<()> {
    let home = config::home()?;
    let debug = std::env::var_os(config::DEBUG).is_some();
    let api_key = secrets::api_key(&home)?;
    let cfg = config::load(&home)?;
    let llm: Arc<dyn Llm> = Arc::new(llm::OpenAiClient::new(&cfg.base_url, api_key, cfg.model, debug)?);
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let log_dir = config::state_dir(&cfg.home).join("sessions").join(started.to_string());
    fs::create_dir_all(&log_dir).with_context(|| format!("cannot create {}", log_dir.display()))?;
    eprintln!("overseer: worker logs in {}", log_dir.display());

    let (sender, queue) = mpsc::channel();
    spawn_stdin_reader(sender.clone());

    let mut fleet = Fleet::new(Arc::clone(&llm), cfg.roots.clone(), cfg.home.clone(), log_dir, sender);
    let mut history = vec![Message::system(prompt::overseer_prompt(&cfg.roots))];
    let tool_defs = prompt::overseer_tool_definitions();
    run_loop(llm.as_ref(), &mut fleet, &mut history, &tool_defs, queue, debug);
    Ok(())
}

/// Reads stdin into `sender` until EOF, mapping each line to an `Event`.
fn spawn_stdin_reader(sender: mpsc::Sender<Event>) {
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Some(event) = parse_line(line.trim()) else { continue };
            let exit = event == Event::Exit;
            if sender.send(event).is_err() || exit {
                return;
            }
        }
        let _ = sender.send(Event::Eof);
    });
}

/// Maps one trimmed stdin line to an event; `None` for a blank line.
fn parse_line(line: &str) -> Option<Event> {
    if line.is_empty() {
        return None;
    }
    if line == "exit" {
        return Some(Event::Exit);
    }
    if let Some(arg) = line.strip_prefix("/resume") {
        return Some(Event::Resume(arg.to_string()));
    }
    Some(Event::User(line.to_string()))
}

/// Drives one overseer turn per event until the queue closes or the user exits.
fn run_loop(
    llm: &dyn Llm,
    fleet: &mut Fleet,
    history: &mut Vec<Message>,
    tool_defs: &Value,
    queue: mpsc::Receiver<Event>,
    debug: bool,
) {
    let mut draining = false;
    for event in queue {
        let input = match event {
            Event::Exit => break,
            Event::Eof => {
                draining = true;
                continue;
            }
            Event::Resume(arg) => {
                println!("{}", fleet.resume(&arg));
                continue;
            }
            Event::Progress { id, text } => {
                eprintln!("[worker {id}] {text}");
                continue;
            }
            Event::User(text) => text,
            Event::Worker { id, kind, text } => {
                fleet.record(id, kind);
                format!("[worker {id} {}] {text}", kind.verb())
            }
        };
        {
            let mut execute = |call: &ToolCall| {
                eprintln!("· {} {}", call.function.name, call.function.arguments);
                let result = fleet.execute(call);
                if debug {
                    eprintln!("  → {result}");
                }
                result
            };
            let mut turn = agent::Turn { llm, tool_defs, execute: &mut execute };
            match turn.run(history, &input) {
                Ok(answer) if !answer.is_empty() => println!("{answer}"),
                Ok(_) => {}
                Err(e) => eprintln!("error: {e:#}"),
            }
        }
        if draining && !fleet.busy() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_classifies_commands() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("exit"), Some(Event::Exit));
        assert_eq!(parse_line("/resume 3"), Some(Event::Resume(" 3".into())));
        assert_eq!(parse_line("hello"), Some(Event::User("hello".into())));
    }
}
