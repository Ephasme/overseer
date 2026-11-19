mod agent;
mod approvals;
mod artifacts;
mod bounds;
mod clipboard;
mod compaction;
mod config;
mod conflict;
mod diff;
mod edit;
mod event;
mod forge;
mod jobs;
mod kernel;
mod llm;
mod mcp;
mod memory;
mod names;
mod pipelines;
mod plain;
mod profiles;
mod prompt;
mod records;
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
use bounds::Bounds;
use event::UiEvent;
use session_store::{ForkAt, Restore, ResumeSource};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use plain::Args;

/// What the command line asks the session to start from. `flags` is the CLI's own name: `mode` is
/// already taken by line mode.
#[derive(Debug, Default, PartialEq)]
enum Session {
    #[default]
    Fresh,
    Continue,
    Resume(Option<String>),
    Fork(String),
}

/// What the run starts from. Cancelled is a clean exit, not a failure: a picker the user backed out
/// of must not look like an error in a `&&` chain or a launcher.
#[derive(Debug)]
enum Start {
    Fresh,
    Restore(Restore),
    Cancelled,
}

/// A session id is one directory name under `state/sessions`: no separators, no relative component.
fn session_dir(state: &std::path::Path, id: &str) -> Result<std::path::PathBuf> {
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

/// Resolves the session a `-c` / `-r` / `--fork` asks for, before anything new is created. `state`
/// is already resolved so a test can point it at a temp dir.
fn resolve_start(state: &std::path::Path, wanted: &Session) -> Result<Start> {
    match wanted {
        Session::Fresh => Ok(Start::Fresh),
        Session::Continue => match session_store::newest(state) {
            Some(info) => Ok(Start::Restore(Restore::Resume(ResumeSource { id: info.id, dir: info.dir }))),
            None => anyhow::bail!("no previous session"),
        },
        Session::Resume(Some(id)) => {
            Ok(Start::Restore(Restore::Resume(ResumeSource { id: id.clone(), dir: session_dir(state, id)? })))
        }
        Session::Resume(None) => {
            if !tui_available(&Args::default()) {
                anyhow::bail!("-r needs a terminal; pass the session id instead");
            }
            match tui::picker::run(state)? {
                Some(id) => {
                    Ok(Start::Restore(Restore::Resume(ResumeSource { id: id.clone(), dir: session_dir(state, &id)? })))
                }
                None => Ok(Start::Cancelled),
            }
        }
        Session::Fork(arg) => {
            let (id, at) = match arg.split_once('@') {
                Some((id, address)) => (id, Some(fork_at(address)?)),
                None => (arg.as_str(), None),
            };
            let dir = session_dir(state, id)?;
            if let Some(at) = at {
                fork_target(&dir, id, at)?;
            }
            Ok(Start::Restore(Restore::Fork { source: ResumeSource { id: id.to_string(), dir }, at }))
        }
    }
}

/// `--checkpoints <id>`: the listing, refused alongside anything that would start a session.
fn checkpoints_listing(dir: &std::path::Path, id: &str, wanted: &Session, args: &Args) -> Result<String> {
    if *wanted != Session::Fresh || args.print.is_some() {
        anyhow::bail!("--checkpoints lists a session; it does not start one");
    }
    session_store::checkpoint_report(dir, id).map_err(anyhow::Error::msg)
}

/// Why a fork address names no state: an absent agent, one a restore does not bring back
/// (stopped, archived or pipeline), or a turn the agent holds no checkpoint for. Turn 0, the
/// empty history before the first commit, always exists.
fn fork_target(dir: &std::path::Path, id: &str, at: ForkAt) -> Result<()> {
    let rows = session_store::rows(dir).map_err(anyhow::Error::msg)?;
    let Some(row) = rows.iter().find(|row| row.id == at.agent) else {
        anyhow::bail!("session {id} has no agent {}", at.agent);
    };
    if row.stopped || row.archived || row.pipeline {
        anyhow::bail!(
            "session {id} agent {} is not restored by a fork: it is stopped, archived or a pipeline",
            at.agent
        );
    }
    let turns = session_store::checkpoint_turns(dir, at.agent).map_err(anyhow::Error::msg)?;
    if at.turn != 0 && !turns.contains(&at.turn) {
        let available = if turns.is_empty() {
            "none".to_string()
        } else {
            turns.iter().map(usize::to_string).collect::<Vec<_>>().join(" ")
        };
        anyhow::bail!("session {id} agent {}: no checkpoint {}; available: {available}", at.agent, at.turn);
    }
    Ok(())
}

/// `<agent>:<turn>`, both numbers; whether they name anything is `fork_target`'s.
fn fork_at(address: &str) -> Result<ForkAt> {
    let invalid = || anyhow::anyhow!("not a checkpoint address: {address:?}; use --fork <session>@<agent>:<turn>");
    let (agent, turn) = address.split_once(':').ok_or_else(&invalid)?;
    Ok(ForkAt { agent: agent.parse().map_err(|_| invalid())?, turn: turn.parse().map_err(|_| invalid())? })
}

/// A CLI flag overrides the file's bound; the merge happens here so every reader of the config —
/// the bounds, the startup notice, `cfg://` — sees the same effective value.
fn apply_flags(cfg: &mut config::Config, args: &Args) {
    cfg.max_time = args.max_time.map(Duration::from_secs).or(cfg.max_time);
    cfg.max_cost = args.max_cost.or(cfg.max_cost);
    cfg.warn_cost = args.warn_cost.or(cfg.warn_cost);
}

fn main() -> Result<()> {
    let (args, wanted) = match parse_args(env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("overseer: {error}");
            eprintln!(
                "usage: overseer [-c | -r [session id] | --fork <session>[@<agent>:<turn>]] [--checkpoints <session id>] [-p <prompt>] [--mode text|json] [--max-time <secs>] [--max-cost <usd>] [--warn-cost <usd>]"
            );
            std::process::exit(2);
        }
    };
    let home = config::home()?;
    let mut cfg = config::load(&home)?;
    apply_flags(&mut cfg, &args);
    let state = config::state_dir(&cfg.home);
    // The listing needs no API key and starts no session: it reads a state dir that already exists.
    if let Some(id) = &args.checkpoints {
        let dir = session_dir(&state, id)?;
        println!("{}", checkpoints_listing(&dir, id, &wanted, &args)?);
        return Ok(());
    }
    let api_key = secrets::api_key(&home)?;
    // Resolved before the new session dir exists, so the picker never lists the session it is
    // about to start. A cancelled picker exits cleanly, before any MCP server or thread came up.
    let restore = match resolve_start(&state, &wanted)? {
        Start::Cancelled => return Ok(()),
        Start::Fresh => None,
        Start::Restore(restore) => Some(restore),
    };
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
    let bounds = Arc::new(Bounds::new(cfg.max_time, cfg.max_cost, cfg.pricing.clone()).with_warn_cost(cfg.warn_cost));
    let models = Arc::new(llm::Backend::new(&cfg.base_url, api_key, cfg.model.clone(), sink, bounds.wants_usage())?);
    let profiles = Arc::new(profiles::Catalogue::load(&cfg.home));
    // A model the pricing table does not name still runs, but its tokens cannot be billed.
    if cfg.max_cost.is_some() || cfg.warn_cost.is_some() {
        let unpriced = config::unpriced(&cfg, &profiles.models());
        if !unpriced.is_empty() {
            let _ = ui_tx
                .send(UiEvent::Notice(format!("no pricing for: {}; their tokens count as $0", unpriced.join(", "))));
        }
    }
    let pipelines = Arc::new(pipelines::Catalogue::load(&cfg.home));

    // Started before the core loop: a server that is slow to connect must not delay startup.
    let mcp = mcp::Registry::start(&cfg.home, cfg.mcp_project, &ui_tx);
    let session_input = input_tx.clone();
    let stop = input_tx.clone();
    let quit = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::clone(&quit);
    let interactive = tui_available(&args);
    let gate = Arc::new(approvals::Gate::new(cfg.approvals, interactive));
    let model = profiles
        .get(profiles::SUPERVISOR)
        .and_then(|profile| profile.model.clone())
        .unwrap_or_else(|| cfg.model.clone());
    // Read before the core takes the config: the palette is fixed before the first frame.
    let theme = cfg.theme;
    let core = thread::spawn(move || {
        session::run(
            models,
            profiles,
            pipelines,
            &cfg,
            log_dir,
            session_input,
            input_rx,
            ui_tx,
            mcp,
            gate,
            shutdown,
            bounds,
            restore,
        )
    });

    let frontend =
        if interactive { tui::run(ui_rx, input_tx, &model, theme) } else { plain::run(ui_rx, input_tx, &args) };
    // A frontend that returned early leaves the core waiting for an event nobody will send.
    quit.store(true, Ordering::Relaxed);
    let _ = stop.send(swarm::Event::Exit);
    // A core that failed, panicked or not, must exit non-zero, not look like a clean quit.
    let session = core.join().map_err(|_| anyhow::anyhow!("the core thread panicked"))?;
    session.and(frontend)
}

/// A terminal on both ends, unless `OVERSEER_PLAIN` asks for line mode anyway, or the run is
/// headless (`-p`/`--print` or an explicit `--mode`, text included), which always takes line mode.
fn tui_available(args: &Args) -> bool {
    args.print.is_none()
        && args.mode.is_none()
        && env::var_os(config::PLAIN).is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

/// The whole CLI: `-p`/`--print`, `--mode`, the two bounds and the session to start from.
/// Hand-rolled, no arg crate.
fn parse_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<(Args, Session)> {
    let mut print: Option<String> = None;
    let mut mode: Option<plain::Mode> = None;
    let mut mode_seen = false;
    let mut max_time: Option<u64> = None;
    let mut max_cost: Option<f64> = None;
    let mut warn_cost: Option<f64> = None;
    let mut checkpoints: Option<String> = None;
    let mut wanted = Session::Fresh;
    let mut items = args.peekable();
    while let Some(arg) = items.next() {
        let arg = arg.into_string().map_err(|_| anyhow::anyhow!("arguments must be UTF-8"))?;
        match arg.as_str() {
            "-p" | "--print" => {
                if print.is_some() {
                    anyhow::bail!("--print given twice");
                }
                // The next argument is the prompt itself, taken literally even when it starts with `-`.
                let value = items
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--print needs a prompt"))?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("arguments must be UTF-8"))?;
                if value.is_empty() {
                    anyhow::bail!("--print needs a non-empty prompt");
                }
                print = Some(value);
            }
            "--print=" => anyhow::bail!("--print needs a non-empty prompt"),
            value if let Some(prompt) = value.strip_prefix("--print=") => {
                if print.is_some() {
                    anyhow::bail!("--print given twice");
                }
                if prompt.is_empty() {
                    anyhow::bail!("--print needs a non-empty prompt");
                }
                print = Some(prompt.to_owned());
            }
            "--mode" => {
                if mode_seen {
                    anyhow::bail!("--mode given twice");
                }
                mode_seen = true;
                let value = items
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--mode needs a value"))?
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("arguments must be UTF-8"))?;
                match value.as_str() {
                    "json" => mode = Some(plain::Mode::Json),
                    "text" => mode = Some(plain::Mode::Text),
                    other => anyhow::bail!("unknown --mode {other:?}: use text or json"),
                }
            }
            "--mode=" => anyhow::bail!("--mode needs a value"),
            value if let Some(m) = value.strip_prefix("--mode=") => {
                if mode_seen {
                    anyhow::bail!("--mode given twice");
                }
                mode_seen = true;
                match m {
                    "" => anyhow::bail!("--mode needs a value"),
                    "json" => mode = Some(plain::Mode::Json),
                    "text" => mode = Some(plain::Mode::Text),
                    other => anyhow::bail!("unknown --mode {other:?}: use text or json"),
                }
            }
            "--max-time" => {
                let value = next_value(&mut items, "--max-time")?;
                max_time = Some(seconds(&value)?);
            }
            "--max-time=" => anyhow::bail!("--max-time needs a value"),
            value if let Some(seconds) = value.strip_prefix("--max-time=") => {
                max_time = Some(self::seconds(seconds)?);
            }
            "--max-cost" => {
                let value = next_value(&mut items, "--max-cost")?;
                max_cost = Some(dollars("--max-cost", &value)?);
            }
            "--max-cost=" => anyhow::bail!("--max-cost needs a value"),
            value if let Some(amount) = value.strip_prefix("--max-cost=") => {
                max_cost = Some(dollars("--max-cost", amount)?);
            }
            "--warn-cost" => {
                let value = next_value(&mut items, "--warn-cost")?;
                warn_cost = Some(dollars("--warn-cost", &value)?);
            }
            "--warn-cost=" => anyhow::bail!("--warn-cost needs a value"),
            value if let Some(amount) = value.strip_prefix("--warn-cost=") => {
                warn_cost = Some(dollars("--warn-cost", amount)?);
            }
            "-c" | "--continue" => wanted = Session::Continue,
            "-r" | "--resume" => {
                // Only a token that is not a flag can be an id: `-r -c` asks for the picker and
                // then continues.
                let id = items.next_if(|next| !next.to_string_lossy().starts_with('-'));
                match id {
                    Some(id) => {
                        let id = id.into_string().map_err(|_| anyhow::anyhow!("arguments must be UTF-8"))?;
                        wanted = Session::Resume(Some(id));
                    }
                    None => wanted = Session::Resume(None),
                }
            }
            "--fork" => {
                let value = next_value(&mut items, "--fork")?;
                wanted = Session::Fork(value);
            }
            "--fork=" => anyhow::bail!("--fork needs a value"),
            value if let Some(target) = value.strip_prefix("--fork=") => {
                wanted = Session::Fork(target.to_owned());
            }
            "--checkpoints" => {
                let value = next_value(&mut items, "--checkpoints")?;
                checkpoints = Some(value);
            }
            "--checkpoints=" => anyhow::bail!("--checkpoints needs a value"),
            value if let Some(id) = value.strip_prefix("--checkpoints=") => {
                checkpoints = Some(id.to_owned());
            }
            other => anyhow::bail!("unexpected argument {other:?}"),
        }
    }
    Ok((Args { print, mode, max_time, max_cost, warn_cost, checkpoints }, wanted))
}

/// The value of a flag given as its own argument; the next item, which may start with `-`.
fn next_value(items: &mut impl Iterator<Item = std::ffi::OsString>, name: &str) -> Result<String> {
    items
        .next()
        .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))?
        .into_string()
        .map_err(|_| anyhow::anyhow!("arguments must be UTF-8"))
}

/// Whole seconds for `--max-time`; zero would trip the bound before any work starts.
fn seconds(value: &str) -> Result<u64> {
    match value.parse::<u64>() {
        Ok(0) => anyhow::bail!("--max-time expects a positive number of seconds, got {value:?}"),
        Ok(seconds) => Ok(seconds),
        Err(_) => anyhow::bail!("--max-time expects seconds, got {value:?}"),
    }
}

/// Dollars for `--max-cost`: a non-finite or non-positive ceiling would never trip, or trip at once.
fn dollars(flag: &str, value: &str) -> Result<f64> {
    match value.parse::<f64>() {
        Ok(cost) if cost.is_finite() && cost > 0.0 => Ok(cost),
        _ => anyhow::bail!("{flag} expects a positive number of dollars, got {value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        parse_args(args.iter().copied().map(std::ffi::OsString::from)).map(|(args, _)| args)
    }

    fn flags(args: &[&str]) -> Result<Session> {
        parse_args(args.iter().copied().map(std::ffi::OsString::from)).map(|(_, wanted)| wanted)
    }

    /// One session dir's `session.jsonl`: a header and the root's agent record.
    fn session_log() -> String {
        let policy = r#"{"allow_tools":["bash"],"read_only":true,"allow_commands":[],"deny_commands":[],"deny_all_commands":false}"#;
        format!(
            "{{\"kind\":\"session\",\"at\":1,\"dir\":\"/tmp\"}}\n{{\"kind\":\"agent\",\"id\":1,\"depth\":0,\"dir\":\"/tmp\",\"task\":\"\",\"profile\":\"supervisor\",\"role\":\"supervisor\",\"policy\":{policy},\"stopped\":false,\"archived\":false,\"pipeline\":false}}\n",
        )
    }

    #[test]
    fn no_flag_is_a_fresh_session() {
        assert_eq!(flags(&[]).unwrap(), Session::Fresh);
    }

    #[test]
    fn the_continue_flags_ask_for_the_newest_session() {
        assert_eq!(flags(&["-c"]).unwrap(), Session::Continue);
        assert_eq!(flags(&["--continue"]).unwrap(), Session::Continue);
    }

    #[test]
    fn resume_takes_an_optional_id() {
        assert_eq!(flags(&["-r"]).unwrap(), Session::Resume(None));
        assert_eq!(flags(&["--resume"]).unwrap(), Session::Resume(None));
        assert_eq!(flags(&["-r", "1700000000-42"]).unwrap(), Session::Resume(Some("1700000000-42".into())));
    }

    #[test]
    fn resume_does_not_swallow_the_next_flag() {
        // `-r -c` asks for the picker and then continues: an id never starts with a dash.
        assert_eq!(flags(&["-r", "-c"]).unwrap(), Session::Continue);
        assert_eq!(flags(&["-r", "--continue"]).unwrap(), Session::Continue);
    }

    #[test]
    fn resume_mixes_with_the_other_flags_in_any_order() {
        let parsed = parse_args(["--mode=json", "-r", "1700000000-42"].into_iter().map(std::ffi::OsString::from));
        let (args, wanted) = parsed.unwrap();
        assert_eq!(args.mode, Some(plain::Mode::Json));
        assert_eq!(wanted, Session::Resume(Some("1700000000-42".into())));
    }

    #[test]
    fn a_missing_session_is_named() {
        let state = tempfile::tempdir().unwrap();
        let err = resolve_start(state.path(), &Session::Resume(Some("nope".into()))).unwrap_err().to_string();
        assert_eq!(err, "no such session: nope");
    }

    #[test]
    fn an_id_cannot_leave_the_state_dir() {
        let state = tempfile::tempdir().unwrap();
        for id in ["../..", "/etc", ".", "..", "", "a/b"] {
            let err = resolve_start(state.path(), &Session::Resume(Some(id.into()))).unwrap_err().to_string();
            assert!(err.starts_with("not a session id:"), "{id:?} gave {err}");
        }
    }

    #[test]
    fn a_session_without_agents_is_not_resumable() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("sessions").join("1700000000-1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session.jsonl"), "{\"kind\":\"session\",\"at\":1,\"dir\":\"/tmp\"}\n").unwrap();
        let err = resolve_start(state.path(), &Session::Resume(Some("1700000000-1".into()))).unwrap_err().to_string();
        assert_eq!(err, "no such session: 1700000000-1");
    }

    #[test]
    fn continue_without_any_session_says_so() {
        let state = tempfile::tempdir().unwrap();
        let err = resolve_start(state.path(), &Session::Continue).unwrap_err().to_string();
        assert_eq!(err, "no previous session");
    }

    #[test]
    fn continue_takes_the_newest_resumable_session() {
        let state = tempfile::tempdir().unwrap();
        let sessions = state.path().join("sessions");
        for id in ["1700000000-1", "1700000001-2"] {
            let dir = sessions.join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("session.jsonl"), session_log()).unwrap();
        }
        // A newer dir with only the header is not resumable and is passed over.
        fs::create_dir_all(sessions.join("1700000009-9")).unwrap();
        fs::write(sessions.join("1700000009-9/session.jsonl"), "{\"kind\":\"session\",\"at\":1,\"dir\":\"/tmp\"}\n")
            .unwrap();

        match resolve_start(state.path(), &Session::Continue).unwrap() {
            Start::Restore(Restore::Resume(source)) => assert_eq!(source.id, "1700000001-2"),
            other => panic!("continue resolved to {other:?}"),
        }
        assert!(matches!(resolve_start(state.path(), &Session::Fresh).unwrap(), Start::Fresh));
    }

    #[test]
    fn fork_takes_a_value_and_the_address_is_parsed_later() {
        assert_eq!(flags(&["--fork", "1700000000-42"]).unwrap(), Session::Fork("1700000000-42".into()));
        assert_eq!(flags(&["--fork=1700000000-42@2:1"]).unwrap(), Session::Fork("1700000000-42@2:1".into()));
        assert!(parse(&["--fork"]).is_err());
        assert!(parse(&["--fork="]).is_err());
    }

    #[test]
    fn checkpoints_takes_a_value() {
        assert_eq!(parse(&["--checkpoints", "1700000000-42"]).unwrap().checkpoints.as_deref(), Some("1700000000-42"));
        assert_eq!(parse(&["--checkpoints=1700000000-42"]).unwrap().checkpoints.as_deref(), Some("1700000000-42"));
        assert!(parse(&["--checkpoints"]).is_err());
        assert!(parse(&["--checkpoints="]).is_err());
    }

    /// A session dir with one agent and one checkpoint, as `--fork` addresses it.
    fn fork_fixture() -> (tempfile::TempDir, String) {
        let state = tempfile::tempdir().unwrap();
        let id = "1700000000-42".to_string();
        let dir = state.path().join("sessions").join(&id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session.jsonl"), session_log()).unwrap();
        session_store::save_checkpoint(&dir, 1, 1, &[]).unwrap();
        (state, id)
    }

    #[test]
    fn a_fork_address_is_refused_before_any_session_starts() {
        let (state, id) = fork_fixture();
        for bad in ["@2", "@2:", "@2:x", "@2:1:3"] {
            let err = resolve_start(state.path(), &Session::Fork(format!("{id}{bad}"))).unwrap_err().to_string();
            assert!(err.starts_with("not a checkpoint address:"), "{bad}: {err}");
        }
        let err = resolve_start(state.path(), &Session::Fork(format!("{id}@7:1"))).unwrap_err().to_string();
        assert_eq!(err, format!("session {id} has no agent 7"));
        let err = resolve_start(state.path(), &Session::Fork(format!("{id}@1:9"))).unwrap_err().to_string();
        assert_eq!(err, format!("session {id} agent 1: no checkpoint 9; available: 1"));
        // A turn inside a hole left by a failed save is refused too, not only one past the newest.
        let dir = state.path().join("sessions").join(&id);
        session_store::save_checkpoint(&dir, 1, 3, &[]).unwrap();
        let err = resolve_start(state.path(), &Session::Fork(format!("{id}@1:2"))).unwrap_err().to_string();
        assert_eq!(err, format!("session {id} agent 1: no checkpoint 2; available: 1 3"));
        assert!(resolve_start(state.path(), &Session::Fork(format!("{id}@1:0"))).is_ok(), "turn 0 always exists");
        let err = resolve_start(state.path(), &Session::Fork("nope".into())).unwrap_err().to_string();
        assert_eq!(err, "no such session: nope");
        match resolve_start(state.path(), &Session::Fork(format!("{id}@1:1"))).unwrap() {
            Start::Restore(Restore::Fork { source, at }) => {
                assert_eq!(source.id, id);
                assert_eq!(at, Some(ForkAt { agent: 1, turn: 1 }));
            }
            other => panic!("a valid fork resolved to {other:?}"),
        }
        // A stopped agent is not restored, so an address naming it would branch nothing.
        let mut log = fs::OpenOptions::new().append(true).open(dir.join("session.jsonl")).unwrap();
        std::io::Write::write_all(&mut log, b"{\"kind\":\"state\",\"id\":1,\"stopped\":true,\"archived\":false}\n")
            .unwrap();
        drop(log);
        let err = resolve_start(state.path(), &Session::Fork(format!("{id}@1:1"))).unwrap_err().to_string();
        assert_eq!(
            err,
            format!("session {id} agent 1 is not restored by a fork: it is stopped, archived or a pipeline")
        );
    }

    #[test]
    fn checkpoints_lists_only_when_nothing_else_starts_a_session() {
        let (state, id) = fork_fixture();
        let dir = state.path().join("sessions").join(&id);
        let args = Args { checkpoints: Some(id.clone()), ..Args::default() };
        let listing = checkpoints_listing(&dir, &id, &Session::Fresh, &args).unwrap();
        assert!(listing.starts_with(&format!("session {id}: 1 agent(s)")), "{listing}");
        assert!(listing.contains(&format!("--fork {id}@1:1")), "{listing}");
        for wanted in [Session::Continue, Session::Resume(Some(id.clone())), Session::Fork(id.clone())] {
            let err = checkpoints_listing(&dir, &id, &wanted, &args).unwrap_err().to_string();
            assert_eq!(err, "--checkpoints lists a session; it does not start one");
        }
        let asked = Args { print: Some("hi".into()), ..Args::default() };
        let err = checkpoints_listing(&dir, &id, &Session::Fresh, &asked).unwrap_err().to_string();
        assert_eq!(err, "--checkpoints lists a session; it does not start one");
    }

    /// The flags a run can carry, and what `apply_flags` does with them: the tested merge is the
    /// one `main` calls, not a copy of it.
    #[test]
    fn the_cost_flags_parse_and_outrank_the_file() {
        let args = parse(&["--warn-cost", "0.5", "--max-cost=2"]).unwrap();
        assert_eq!((args.warn_cost, args.max_cost), (Some(0.5), Some(2.0)));
        assert_eq!(parse(&["--warn-cost=0.25"]).unwrap().warn_cost, Some(0.25));
        for bad in ["--warn-cost", "--warn-cost=", "--warn-cost=0", "--warn-cost=-1", "--warn-cost=inf"] {
            assert!(parse(&[bad]).is_err(), "{bad} was accepted");
        }
        // A flag wins, and only the flags given: a key the command line omits keeps the file's value.
        let mut cfg = config::Config {
            roots: Vec::new(),
            base_url: String::new(),
            model: "m".into(),
            theme: config::Theme::Auto,
            max_depth: 0,
            dir: std::path::PathBuf::new(),
            rule_dir: std::path::PathBuf::new(),
            home: std::path::PathBuf::new(),
            approvals: crate::approvals::Tier::Ask,
            mcp_project: config::McpProject::On,
            max_time: None,
            max_cost: Some(9.0),
            warn_cost: Some(3.0),
            pricing: std::collections::HashMap::new(),
            compaction: config::CompactionSettings::default(),
        };
        apply_flags(&mut cfg, &parse(&["--warn-cost", "0.5"]).unwrap());
        assert_eq!((cfg.warn_cost, cfg.max_cost), (Some(0.5), Some(9.0)));
    }

    #[test]
    fn no_args_is_line_mode() {
        assert_eq!(parse(&[]).unwrap(), Args { print: None, mode: None, ..Args::default() });
    }

    #[test]
    fn short_and_long_print_agree() {
        let short = parse(&["-p", "hi"]).unwrap();
        let long = parse(&["--print", "hi"]).unwrap();
        assert_eq!(short, long);
        assert_eq!(short.print.as_deref(), Some("hi"));
    }

    #[test]
    fn print_eq_takes_the_rest_verbatim() {
        let args = parse(&["--print=--mode json"]).unwrap();
        assert_eq!(args.print.as_deref(), Some("--mode json"));
    }

    #[test]
    fn print_takes_the_next_argument_even_a_flag() {
        // `-p --mode` prompts with `--mode`; then `json` has nowhere to belong.
        assert!(parse(&["-p", "--mode", "json"]).is_err());
    }

    #[test]
    fn short_flag_rejects_the_eq_form() {
        assert!(parse(&["-p=hi"]).is_err());
    }

    #[test]
    fn print_without_a_value_is_an_error() {
        assert!(parse(&["-p"]).is_err());
        assert!(parse(&["--print"]).is_err());
        assert!(parse(&["--print="]).is_err());
    }

    #[test]
    fn empty_prompt_is_an_error() {
        assert!(parse(&["-p", ""]).is_err());
    }

    #[test]
    fn duplicated_print_is_an_error() {
        assert!(parse(&["-p", "a", "--print", "b"]).is_err());
    }

    #[test]
    fn mode_json_and_mode_text() {
        assert_eq!(
            parse(&["--mode", "json"]).unwrap(),
            Args { print: None, mode: Some(plain::Mode::Json), ..Args::default() }
        );
        assert_eq!(
            parse(&["--mode=json"]).unwrap(),
            Args { print: None, mode: Some(plain::Mode::Json), ..Args::default() }
        );
        assert_eq!(
            parse(&["--mode", "text"]).unwrap(),
            Args { print: None, mode: Some(plain::Mode::Text), ..Args::default() }
        );
        assert_eq!(
            parse(&["--mode=text"]).unwrap(),
            Args { print: None, mode: Some(plain::Mode::Text), ..Args::default() }
        );
    }

    #[test]
    fn mode_needs_a_value() {
        assert!(parse(&["--mode"]).is_err());
        assert!(parse(&["--mode="]).is_err());
    }

    #[test]
    fn unknown_mode_is_an_error() {
        assert!(parse(&["--mode", "xml"]).is_err());
    }

    #[test]
    fn duplicated_mode_is_an_error() {
        assert!(parse(&["--mode", "json", "--mode", "text"]).is_err());
    }

    #[test]
    fn stray_positional_is_an_error() {
        assert!(parse(&["hello"]).is_err());
        assert!(parse(&["--verbose"]).is_err());
    }

    #[test]
    fn order_independent() {
        assert_eq!(parse(&["--mode", "json", "-p", "hi"]).unwrap(), parse(&["-p", "hi", "--mode", "json"]).unwrap());
    }

    #[test]
    fn non_utf8_argument_is_an_error() {
        let args = [std::ffi::OsString::from("--mode"), std::os::unix::ffi::OsStringExt::from_vec(vec![0xff])];
        assert!(parse_args(args.into_iter()).is_err());
    }
}
