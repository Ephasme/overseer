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
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

use plain::Args;

fn main() -> Result<()> {
    let args = match parse_args(env::args_os().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("overseer: {error}");
            eprintln!("usage: overseer [-p <prompt> | --print <prompt>] [--mode text|json]");
            std::process::exit(2);
        }
    };
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
    let models = Arc::new(llm::Backend::new(&cfg.base_url, api_key, cfg.model.clone(), sink)?);
    let profiles = Arc::new(profiles::Catalogue::load(&cfg.home));
    let pipelines = Arc::new(pipelines::Catalogue::load(&cfg.home));

    // Started before the core loop: a server that is slow to connect must not delay startup.
    let mcp = mcp::Registry::start(&cfg.home, cfg.mcp_project, &ui_tx);
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
        session::run(models, profiles, pipelines, &cfg, log_dir, session_input, input_rx, ui_tx, mcp, shutdown)
    });

    let frontend = if tui_available(&args) {
        tui::run(ui_rx, input_tx, &model, theme)
    } else {
        plain::run(ui_rx, input_tx, &args)
    };
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

/// The whole CLI: only `-p`/`--print` and `--mode`. Hand-rolled, no arg crate.
fn parse_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Args> {
    let mut print: Option<String> = None;
    let mut mode: Option<plain::Mode> = None;
    let mut mode_seen = false;
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
            other => anyhow::bail!("unexpected argument {other:?}"),
        }
    }
    Ok(Args { print, mode })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        parse_args(args.iter().copied().map(std::ffi::OsString::from))
    }

    #[test]
    fn no_args_is_line_mode() {
        assert_eq!(parse(&[]).unwrap(), Args { print: None, mode: None });
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
        assert_eq!(parse(&["--mode", "json"]).unwrap(), Args { print: None, mode: Some(plain::Mode::Json) });
        assert_eq!(parse(&["--mode=json"]).unwrap(), Args { print: None, mode: Some(plain::Mode::Json) });
        assert_eq!(parse(&["--mode", "text"]).unwrap(), Args { print: None, mode: Some(plain::Mode::Text) });
        assert_eq!(parse(&["--mode=text"]).unwrap(), Args { print: None, mode: Some(plain::Mode::Text) });
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
