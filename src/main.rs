mod agent;
mod artifacts;
mod clipboard;
mod config;
mod conflict;
mod diff;
mod edit;
mod event;
mod forge;
mod jobs;
mod llm;
mod mcp;
mod memory;
mod names;
mod pipelines;
mod plain;
mod profiles;
mod prompt;
mod repos;
mod rules;
mod secrets;
mod session;
mod session_store;
mod skills;
mod slash;
mod swarm;
mod tools;
mod tui;
mod url;
mod vault;
mod version;

use anyhow::{Context, Result};
use event::UiEvent;
use session_store::ResumeSource;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

/// What the command line asks for: a fresh session, the newest one, or a named/picked one.
#[derive(Debug, PartialEq)]
enum Mode {
    Fresh,
    Continue,
    Resume(Option<String>),
}

fn parse(argv: &[String]) -> Result<Mode> {
    let mut mode = Mode::Fresh;
    let mut rest = argv.iter().peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-c" | "--continue" => mode = Mode::Continue,
            "-r" | "--resume" => {
                let id = rest.next().cloned();
                mode = Mode::Resume(id);
            }
            other => anyhow::bail!("unknown argument: {other}\nusage: overseer [-c | -r [session-id]]"),
        }
    }
    Ok(mode)
}

/// Resolves the session a `-c` / `-r` asks for, before anything new is created.
fn resume_source(home: &std::path::Path, mode: &Mode) -> Result<Option<ResumeSource>> {
    let state = config::state_dir(home);
    match mode {
        Mode::Fresh => Ok(None),
        Mode::Continue => {
            let newest = session_store::sessions(&state).into_iter().next();
            match newest {
                Some(info) => Ok(Some(ResumeSource { id: info.id, dir: info.dir })),
                None => anyhow::bail!("no previous session"),
            }
        }
        Mode::Resume(Some(id)) => {
            let dir = state.join("sessions").join(id);
            if !dir.join("session.jsonl").is_file() {
                anyhow::bail!("no such session: {id}");
            }
            Ok(Some(ResumeSource { id: id.clone(), dir }))
        }
        Mode::Resume(None) => {
            if !tui_available() {
                anyhow::bail!("-r needs a terminal; pass the session id instead");
            }
            let picked = tui::picker::run(home)?;
            match picked {
                Some(id) => {
                    let dir = state.join("sessions").join(&id);
                    Ok(Some(ResumeSource { id, dir }))
                }
                None => Err(anyhow::Error::msg("__cancelled__")),
            }
        }
    }
}

fn main() -> Result<()> {
    let mode = parse(&env::args().skip(1).collect::<Vec<_>>())?;
    let home = config::home()?;
    let cfg = config::load(&home)?;
    // Resolved before the new log dir exists, so the picker never sees the session it is about to
    // start. A cancelled picker exits cleanly, before any MCP server or thread came up.
    let resume = match resume_source(&cfg.home, &mode) {
        Ok(resume) => resume,
        Err(e) if e.to_string() == "__cancelled__" => return Ok(()),
        Err(e) => return Err(e),
    };
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
    let models = Arc::new(llm::Backend::new(&cfg.base_url, api_key, cfg.model.clone(), sink)?);
    let profiles = Arc::new(profiles::Catalogue::load(&cfg.home));
    let pipelines = Arc::new(pipelines::Catalogue::load(&cfg.home));

    // Started before the core loop: a server that is slow to connect must not delay startup.
    let mcp = mcp::Registry::start(&cfg.home, &ui_tx);
    let session_input = input_tx.clone();
    let stop = input_tx.clone();
    let quit = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::clone(&quit);
    let model = profiles
        .get(profiles::SUPERVISOR)
        .and_then(|profile| profile.model.clone())
        .unwrap_or_else(|| cfg.model.clone());
    // Read before the core takes the config: the palette is fixed before the first frame.
    let theme = cfg.theme;
    let core = thread::spawn(move || {
        session::run(models, profiles, pipelines, &cfg, log_dir, session_input, input_rx, ui_tx, mcp, shutdown, resume)
    });

    let frontend = if tui_available() {
        tui::run(ui_rx, input_tx, &model, theme)
    } else {
        plain::run(ui_rx, input_tx);
        Ok(())
    };
    // A frontend that returned early leaves the core waiting for an event nobody will send.
    quit.store(true, Ordering::Relaxed);
    let _ = stop.send(swarm::Event::Exit);
    // A core that failed, panicked or not, must exit non-zero, not look like a clean quit.
    let session = core.join().map_err(|_| anyhow::anyhow!("the core thread panicked"))?;
    session.and(frontend)
}

/// A terminal on both ends, unless `OVERSEER_PLAIN` asks for line mode anyway.
fn tui_available() -> bool {
    env::var_os(config::PLAIN).is_none() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn no_flag_is_a_fresh_session() {
        assert_eq!(parse(&[]).unwrap(), Mode::Fresh);
    }

    #[test]
    fn the_continue_flags_ask_for_the_newest_session() {
        assert_eq!(parse(&args(&["-c"])).unwrap(), Mode::Continue);
        assert_eq!(parse(&args(&["--continue"])).unwrap(), Mode::Continue);
    }

    #[test]
    fn resume_takes_an_optional_id() {
        assert_eq!(parse(&args(&["-r"])).unwrap(), Mode::Resume(None));
        assert_eq!(parse(&args(&["--resume"])).unwrap(), Mode::Resume(None));
        assert_eq!(parse(&args(&["-r", "1700000000-42"])).unwrap(), Mode::Resume(Some("1700000000-42".into())));
    }

    #[test]
    fn an_unknown_flag_names_the_usage() {
        let err = parse(&args(&["-x"])).unwrap_err().to_string();
        assert!(err.contains("usage: overseer [-c | -r [session-id]]"), "{err}");
    }

    #[test]
    fn a_positional_argument_is_refused() {
        assert!(parse(&args(&["extra"])).is_err());
    }

    #[test]
    fn a_missing_session_is_named() {
        let home = tempfile::tempdir().unwrap();
        let err = resume_source(home.path(), &Mode::Resume(Some("nope".into()))).unwrap_err().to_string();
        assert_eq!(err, "no such session: nope");
    }

    #[test]
    fn continue_without_any_session_says_so() {
        let home = tempfile::tempdir().unwrap();
        let err = resume_source(home.path(), &Mode::Continue).unwrap_err().to_string();
        assert_eq!(err, "no previous session");
    }
}
