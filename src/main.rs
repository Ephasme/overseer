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

/// What the run starts from. Cancelled is a clean exit, not a failure: a picker the user backed out
/// of must not look like an error in a `&&` chain or a launcher.
#[derive(Debug)]
enum Start {
    Fresh,
    Resume(ResumeSource),
    Cancelled,
}

fn parse(argv: &[String]) -> Result<Mode> {
    let mut mode = Mode::Fresh;
    let mut rest = argv.iter().peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-c" | "--continue" => mode = Mode::Continue,
            "-r" | "--resume" => {
                // Only a token that is not a flag can be an id: `-r -c` asks for the picker, then
                // reads `-c` as its own flag.
                let id = rest.next_if(|next| !next.starts_with('-')).cloned();
                mode = Mode::Resume(id);
            }
            other => anyhow::bail!("unknown argument: {other}\nusage: overseer [-c | -r [session-id]]"),
        }
    }
    Ok(mode)
}

/// A session id is one directory name under `state/sessions`: no separators, no relative component.
fn session_dir(state: &std::path::Path, id: &str) -> Result<std::path::PathBuf, anyhow::Error> {
    let invalid = id.is_empty() || id == "." || id == ".." || id.contains('/') || id.contains('\\');
    if invalid {
        anyhow::bail!("not a session id: {id:?}");
    }
    let dir = state.join("sessions").join(id);
    if !session_store::resumable(&dir) {
        anyhow::bail!("no such session: {id}");
    }
    Ok(dir)
}

/// Resolves the session a `-c` / `-r` asks for, before anything new is created. `state` is already
/// resolved so a test can point it at a temp dir.
fn resume_source(state: &std::path::Path, mode: &Mode) -> Result<Start> {
    match mode {
        Mode::Fresh => Ok(Start::Fresh),
        Mode::Continue => match session_store::newest(state) {
            Some(info) => Ok(Start::Resume(ResumeSource { id: info.id, dir: info.dir })),
            None => anyhow::bail!("no previous session"),
        },
        Mode::Resume(Some(id)) => Ok(Start::Resume(ResumeSource { id: id.clone(), dir: session_dir(state, id)? })),
        Mode::Resume(None) => {
            if !tui_available() {
                anyhow::bail!("-r needs a terminal; pass the session id instead");
            }
            match tui::picker::run(state)? {
                Some(id) => Ok(Start::Resume(ResumeSource { id: id.clone(), dir: session_dir(state, &id)? })),
                None => Ok(Start::Cancelled),
            }
        }
    }
}

fn main() -> Result<()> {
    let mode = parse(&env::args().skip(1).collect::<Vec<_>>())?;
    let home = config::home()?;
    let cfg = config::load(&home)?;
    let state = config::state_dir(&cfg.home);
    // Resolved before the new log dir exists, so the picker never sees the session it is about to
    // start. A cancelled picker exits cleanly, before any MCP server or thread came up.
    let resume = match resume_source(&state, &mode)? {
        Start::Cancelled => return Ok(()),
        Start::Fresh => None,
        Start::Resume(source) => Some(source),
    };
    let api_key = secrets::api_key(&home)?;
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    // Two runs started in the same second must not share, and truncate, each other's worker logs.
    let log_dir = state.join("sessions").join(format!("{started}-{}", std::process::id()));
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
    fn resume_does_not_swallow_the_next_flag() {
        // `-r -c` asks for the picker and then continues: an id never starts with a dash.
        assert_eq!(parse(&args(&["-r", "-c"])).unwrap(), Mode::Continue);
        assert_eq!(parse(&args(&["-r", "--continue"])).unwrap(), Mode::Continue);
    }

    #[test]
    fn a_missing_session_is_named() {
        let state = tempfile::tempdir().unwrap();
        let err = resume_source(state.path(), &Mode::Resume(Some("nope".into()))).unwrap_err().to_string();
        assert_eq!(err, "no such session: nope");
    }

    #[test]
    fn an_id_cannot_leave_the_state_dir() {
        let state = tempfile::tempdir().unwrap();
        for id in ["../..", "/etc", ".", "..", "", "a/b"] {
            let err = resume_source(state.path(), &Mode::Resume(Some(id.into()))).unwrap_err().to_string();
            assert!(err.starts_with("not a session id:"), "{id:?} gave {err}");
        }
    }

    #[test]
    fn a_session_without_agents_is_not_resumable() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("sessions").join("1700000000-1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session.jsonl"), "{\"kind\":\"session\",\"at\":1,\"dir\":\"/tmp\"}\n").unwrap();
        let err = resume_source(state.path(), &Mode::Resume(Some("1700000000-1".into()))).unwrap_err().to_string();
        assert_eq!(err, "no such session: 1700000000-1");
    }

    #[test]
    fn continue_without_any_session_says_so() {
        let state = tempfile::tempdir().unwrap();
        let err = resume_source(state.path(), &Mode::Continue).unwrap_err().to_string();
        assert_eq!(err, "no previous session");
    }

    #[test]
    fn continue_takes_the_newest_resumable_session() {
        let state = tempfile::tempdir().unwrap();
        let sessions = state.path().join("sessions");
        for id in ["1700000000-1", "1700000001-2"] {
            let dir = sessions.join(id);
            fs::create_dir_all(&dir).unwrap();
            let policy = r#"{"allow_tools":["bash"],"read_only":true,"allow_commands":[],"deny_commands":[],"deny_all_commands":false}"#;
            fs::write(
                dir.join("session.jsonl"),
                format!(
                    "{{\"kind\":\"session\",\"at\":1,\"dir\":\"/tmp\"}}\n{{\"kind\":\"agent\",\"id\":1,\"depth\":0,\"dir\":\"/tmp\",\"task\":\"\",\"profile\":\"supervisor\",\"role\":\"supervisor\",\"policy\":{policy},\"stopped\":false,\"archived\":false,\"pipeline\":false}}\n"
                ),
            )
            .unwrap();
        }
        match resume_source(state.path(), &Mode::Continue).unwrap() {
            Start::Resume(source) => assert_eq!(source.id, "1700000001-2"),
            other => panic!("continue resolved to {other:?}"),
        }
        assert!(matches!(resume_source(state.path(), &Mode::Fresh).unwrap(), Start::Fresh));
    }
}
