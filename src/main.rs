mod agent;
mod config;
mod fleet;
mod llm;
mod tools;

use anyhow::{Context, Result};
use fleet::{Event, Fleet};
use llm::{Llm, Message, ToolCall};
use std::io::{self, BufRead};
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

fn main() -> Result<()> {
    let api_key = env::var("OVERSEER_API_KEY").context("OVERSEER_API_KEY is not set")?;
    let cfg = config::load(&config::path())?;
    let llm: Arc<dyn Llm> = Arc::new(llm::OpenAiClient::new(&cfg.base_url, api_key, cfg.model)?);
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let log_dir = config::state_dir().join("sessions").join(started.to_string());
    fs::create_dir_all(&log_dir).with_context(|| format!("cannot create {}", log_dir.display()))?;
    eprintln!("overseer: worker logs in {}", log_dir.display());

    let (sender, queue) = mpsc::channel();
    let stdin_sender = sender.clone();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event = if line == "exit" { Event::Exit } else { Event::User(line.to_string()) };
            let exit = event == Event::Exit;
            if stdin_sender.send(event).is_err() || exit {
                return;
            }
        }
        let _ = stdin_sender.send(Event::Eof);
    });

    let mut fleet = Fleet::new(Arc::clone(&llm), cfg.roots.clone(), log_dir, sender);
    let mut history = vec![Message::system(fleet::overseer_prompt(&cfg.roots))];
    let tool_defs = fleet::overseer_tool_definitions();
    let mut draining = false;
    for event in queue {
        let input = match event {
            Event::Exit => break,
            Event::Eof => {
                draining = true;
                if fleet.busy() {
                    continue;
                }
                break;
            }
            Event::User(text) => text,
            Event::Worker { id, kind, text } => {
                fleet.record(id, kind);
                format!("[worker {id} {}] {text}", kind.verb())
            }
        };
        let mut execute = |call: &ToolCall| {
            eprintln!("· {} {}", call.function.name, call.function.arguments);
            fleet.execute(call)
        };
        match agent::run_turn(llm.as_ref(), &mut history, &input, &tool_defs, &mut execute) {
            Ok(answer) if !answer.is_empty() => println!("{answer}"),
            Ok(_) => {}
            Err(e) => eprintln!("error: {e:#}"),
        }
        if draining && !fleet.busy() {
            break;
        }
    }
    Ok(())
}
