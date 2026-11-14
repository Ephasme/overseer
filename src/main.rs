mod agent;
mod clipboard;
mod config;
mod conflict;
mod event;
mod fleet;
mod llm;
mod plain;
mod prompt;
mod repos;
mod secrets;
mod session;
mod tools;
mod tui;
mod url;
mod version;

use anyhow::{Context, Result};
use event::UiEvent;
use std::io::IsTerminal;
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

fn main() -> Result<()> {
    let home = config::home()?;
    let cfg = config::load(&home)?;
    let api_key = secrets::api_key(&home)?;
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    // Two runs started in the same second must not share, and truncate, each other's worker logs.
    let log_dir = config::state_dir(&cfg.home).join("sessions").join(format!("{started}-{}", std::process::id()));
    fs::create_dir_all(&log_dir).with_context(|| format!("cannot create {}", log_dir.display()))?;
    let (input_tx, input_rx) = mpsc::channel();
    let (ui_tx, ui_rx) = mpsc::channel();
    let debug = env::var_os(config::DEBUG).is_some();
    let sink: Option<llm::DebugSink> = if debug {
        let ui = ui_tx.clone();
        let sink: llm::DebugSink = Arc::new(move |line: &str| {
            let _ = ui.send(UiEvent::Debug(line.to_string()));
        });
        Some(sink)
    } else {
        None
    };
    let llm: Arc<dyn llm::Llm> = Arc::new(llm::OpenAiClient::new(&cfg.base_url, api_key, cfg.model.clone(), sink)?);

    let session_input = input_tx.clone();
    let stop = input_tx.clone();
    let model = cfg.model.clone();
    let core = thread::spawn(move || session::run(llm, &cfg, log_dir, session_input, input_rx, ui_tx));

    let frontend = if tui_available() {
        tui::run(ui_rx, input_tx, &model)
    } else {
        plain::run(ui_rx, input_tx);
        Ok(())
    };
    // A frontend that returned early leaves the core waiting for an event nobody will send.
    let _ = stop.send(fleet::Event::Exit);
    // A panicking core must exit non-zero, not look like a clean quit.
    core.join().map_err(|_| anyhow::anyhow!("the core thread panicked"))?;
    frontend
}

/// A terminal on both ends, unless `OVERSEER_PLAIN` asks for line mode anyway.
fn tui_available() -> bool {
    env::var_os(config::PLAIN).is_none() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
