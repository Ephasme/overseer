# Agent Instructions

## Critical constraints

- The overseer agent never does work itself: `prompt::overseer_tool_definitions` holds only fleet tools. Never give it `bash`, `read_file`, `write_file` or any other tool that touches files or runs commands; `list_repos`, which only names directories under the configured roots, is the one read the overseer gets.
- Each worker starts in an existing directory: `config::resolve_dir` canonicalises it and rejects a non-directory. `roots` in `~/.config/overseer/config.toml` bounds only repository discovery (`list_repos` and the overseer prompt); it is not a sandbox. Never add a tool that clones a repository. Workers are not sandboxed, so `bash` and absolute paths reach the whole filesystem.
- Release only by pushing a `v<version>` tag that equals `version` in `Cargo.toml`; CI rejects any other tag, publishes the GitHub release and updates `Ephasme/homebrew-tap`.

## Commands

| Task | Command |
|------|---------|
| Lints (CI: warnings are errors) | `cargo clippy --all-targets -- -D warnings` |
| All tests | `cargo nextest run` |
| One test | `cargo nextest run <test_name>` |
| Format | `cargo fmt --all` |
| Unused dependencies | `cargo shear` |
| Advisories, licences, sources | `cargo deny check` |
| Spelling | `typos` |
| Build and install locally, ahead of Homebrew's copy | `just install` |
| Every check CI runs | `just check` |

## Layout

| Path | Contents |
|------|----------|
| `src/main.rs` | picks the frontend, creates both channels, starts the session thread |
| `src/session.rs` | the blocking core loop: one overseer turn per event, reported as `UiEvent`s |
| `src/event.rs` | `UiEvent`, everything the core asks a frontend to show |
| `src/plain.rs` | line-mode frontend: stdin lines in, plain lines out |
| `src/tui/mod.rs` | terminal lifecycle, the event loop, key bindings |
| `src/tui/app.rs` | transcript entries, composer, worker panel, status line, wrapping |
| `src/tui/theme.rs` | the palette: neutral ramp, three hues, status colours |
| `src/fleet.rs` | worker registry, worker threads, worker events, overseer tool dispatch |
| `src/prompt.rs` | both system prompts, tool schemas, tool-name constants, `DEFAULT_WORKDIR` |
| `src/repos.rs` | git-repository discovery under the configured roots |
| `src/secrets.rs` | `OVERSEER_API_KEY` from the environment or `~/.config/overseer/.env` |
| `src/agent.rs` | `Turn::run`, the model ↔ tools loop shared by overseer and workers |
| `src/tools.rs` | worker tools `bash`, `read_file`, `write_file` |
| `src/conflict.rs` | the conflict registry: numbered merge-conflict blocks a worker resolves by `conflict://<N>` |
| `src/llm.rs` | chat-completions wire types, SSE streaming, `StreamLlm`, `testing::Scripted` |
| `src/config.rs` | `config.toml`, roots, `home`, `resolve_dir`, state directory, `io_error` |
| `.github/workflows/ci.yml` | checks; on a `v*` tag: build, release, Homebrew formula update |
| `.github/scripts/update-formula.sh` | rewrites the tap's formula from a release |
| `justfile` | `just install`, `just run`, `just check` |

## Key conventions

- Handle errors with `Result`; outside tests, `unwrap`, `expect`, `panic!`, slice indexing and `as` casts fail the lints.
- A tool failure goes back to the model as text starting `error: `; it never aborts a turn.
- Tests run offline: script the model with `llm::testing::Scripted`, never call a real endpoint. nextest kills a test after 60 s (`.config/nextest.toml`).
- Tool arguments are typed structs with `deny_unknown_fields`: a wrong key or a missing field comes back to the model as `error: invalid arguments: …`, naming the allowed fields.
- A worker's `bash` runs in its own process group and is killed after 300 s (`tools::BASH_TIMEOUT`): without the group kill, a surviving grandchild holds the pipes and the tool call never returns.
- An overseer order reaches a worker through `fleet::Control`: pause holds the inbox, stop drops it, urgent jumps the queue and sets the flag `agent::Turn` checks before every model call and every tool call. A paused worker is not busy, so ctrl+D stops waiting on it.
- A worker's `Policy` is checked on every tool call and rewritten into its system prompt each turn, so a restriction takes effect without a restart. `read_only` matches `bash` commands as text against a fixed list: a guard, not a sandbox.
- A conflict id belongs to one worker's `conflict::Registry`, not the process: ids never collide across workers, and a resolved id stays dead so a stale write fails loudly. A resolution splices out the marker lines only, keeping the code around them.
- Quitting calls `tools::kill_running`: without it a worker's `cargo build` outlives the process, because the session never reaps a group that is still running.
- A worker's liveness is its thread's `JoinHandle`, not a bookkeeping counter: a panicked worker cannot leave the fleet busy forever.
- The core never prints: everything visible is a `UiEvent`, and only a frontend touches the terminal. `Event` is the other direction, frontend to core.
- Worker model text streams through `llm::StreamLlm` into `Event::Delta`, which the TUI shows in the worker panel and the core never feeds back to the overseer.
- A worker view owns the keys while it is open (`tui::app::App::worker_key`): `esc` leaves it and never quits, and no key typed there reaches the overseer's prompt. Quitting stays on ctrl+c and ctrl+d.
- A `UiEvent` carrying a worker id goes to that worker's `Stream` only; the lead's stream (`App::lead`) gets one lifecycle line per worker event and never a worker's tool call or message. `Stream` holds the entries, the flatten cache and the scroll position, so switching views loses neither buffer nor position. F3 is the one verbosity flag: it re-flows every stream.
- Worker tool calls emit `Event::Progress` (start, then elapsed); it wakes nothing.
- Debugging a real run: `OVERSEER_DEBUG=1` echoes each model request and every tool result. Prefer `just run`: an interactive shell may hash Homebrew's `overseer` under the same name and silently run the released binary.
- Code, comments, commits and PRs in English.
