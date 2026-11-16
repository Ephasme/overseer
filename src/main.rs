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
use session_store::ResumeSource;
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
}

/// What the run starts from. Cancelled is a clean exit, not a failure: a picker the user backed out
/// of must not look like an error in a `&&` chain or a launcher.
#[derive(Debug)]
enum Start {
    Fresh,
    Resume(ResumeSource),
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

/// Resolves the session a `-c` / `-r` asks for, before anything new is created. `state` is already
/// resolved so a test can point it at a temp dir.
fn resume_source(state: &std::path::Path, wanted: &Session) -> Result<Start> {
    match wanted {
        Session::Fresh => Ok(Start::Fresh),
        Session::Continue => match session_store::newest(state) {
            Some(info) => Ok(Start::Resume(ResumeSource { id: info.id, dir: info.dir })),
            None => anyhow::bail!("no previous session"),
        },
        Session::Resume(Some(id)) => Ok(Start::Resume(ResumeSource { id: id.clone(), dir: session_dir(state, id)? })),
        Session::Resume(None) => {
            if !tui_available(&Args::default()) {
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
    let (args, wanted) = match parse_args(env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("overseer: {error}");
            eprintln!(
                "usage: overseer [-c | -r [session id]] [-p <prompt>] [--mode text|json] [--max-time <secs>] [--max-cost <usd>]"
            );
            std::process::exit(2);
        }
    };
    let flag_time = args.max_time.map(Duration::from_secs);
    let flag_cost = args.max_cost;
    let home = config::home()?;
    let cfg = config::load(&home)?;
    // A CLI flag overrides the file's bound; the merge happens here so every reader of `cfg` —
    // the bounds, the startup notice, `cfg://` — sees the same effective value.
    let mut cfg = cfg;
    cfg.max_time = flag_time.or(cfg.max_time);
    cfg.max_cost = flag_cost.or(cfg.max_cost);
    let api_key = secrets::api_key(&home)?;
    let state = config::state_dir(&cfg.home);
    // Resolved before the new session dir exists, so the picker never lists the session it is
    // about to start. A cancelled picker exits cleanly, before any MCP server or thread came up.
    let resume = match resume_source(&state, &wanted)? {
        Start::Cancelled => return Ok(()),
        Start::Fresh => None,
        Start::Resume(source) => Some(source),
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
    let bounds = Arc::new(Bounds::new(cfg.max_time, cfg.max_cost, cfg.pricing.clone()));
    let models =
        Arc::new(llm::Backend::new(&cfg.base_url, api_key, cfg.model.clone(), sink, Some(Arc::clone(&bounds)))?);
    let profiles = Arc::new(profiles::Catalogue::load(&cfg.home));
    // A model the pricing table does not name still runs, but its tokens cannot be billed.
    if cfg.max_cost.is_some() {
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
            resume,
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
                max_cost = Some(dollars(&value)?);
            }
            "--max-cost=" => anyhow::bail!("--max-cost needs a value"),
            value if let Some(dollars) = value.strip_prefix("--max-cost=") => {
                max_cost = Some(self::dollars(dollars)?);
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
            other => anyhow::bail!("unexpected argument {other:?}"),
        }
    }
    Ok((Args { print, mode, max_time, max_cost }, wanted))
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
fn dollars(value: &str) -> Result<f64> {
    match value.parse::<f64>() {
        Ok(cost) if cost.is_finite() && cost > 0.0 => Ok(cost),
        _ => anyhow::bail!("--max-cost expects a positive number of dollars, got {value:?}"),
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
        let err = resume_source(state.path(), &Session::Resume(Some("nope".into()))).unwrap_err().to_string();
        assert_eq!(err, "no such session: nope");
    }

    #[test]
    fn an_id_cannot_leave_the_state_dir() {
        let state = tempfile::tempdir().unwrap();
        for id in ["../..", "/etc", ".", "..", "", "a/b"] {
            let err = resume_source(state.path(), &Session::Resume(Some(id.into()))).unwrap_err().to_string();
            assert!(err.starts_with("not a session id:"), "{id:?} gave {err}");
        }
    }

    #[test]
    fn a_session_without_agents_is_not_resumable() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("sessions").join("1700000000-1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session.jsonl"), "{\"kind\":\"session\",\"at\":1,\"dir\":\"/tmp\"}\n").unwrap();
        let err = resume_source(state.path(), &Session::Resume(Some("1700000000-1".into()))).unwrap_err().to_string();
        assert_eq!(err, "no such session: 1700000000-1");
    }

    #[test]
    fn continue_without_any_session_says_so() {
        let state = tempfile::tempdir().unwrap();
        let err = resume_source(state.path(), &Session::Continue).unwrap_err().to_string();
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

        match resume_source(state.path(), &Session::Continue).unwrap() {
            Start::Resume(source) => assert_eq!(source.id, "1700000001-2"),
            other => panic!("continue resolved to {other:?}"),
        }
        assert!(matches!(resume_source(state.path(), &Session::Fresh).unwrap(), Start::Fresh));
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
