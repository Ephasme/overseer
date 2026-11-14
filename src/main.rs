mod agent;
mod config;
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
    let log_dir = config::state_dir(&cfg.home).join("sessions").join(started.to_string());
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
    let model = cfg.model.clone();
    thread::spawn(move || session::run(llm, &cfg, log_dir, session_input, input_rx, ui_tx));

    if tui_available() {
        tui::run(ui_rx, input_tx, &model)
    } else {
        plain::run(ui_rx, input_tx);
        Ok(())
    }
}

/// A terminal on both ends, unless `OVERSEER_PLAIN` asks for line mode anyway.
fn tui_available() -> bool {
    env::var_os(config::PLAIN).is_none() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
