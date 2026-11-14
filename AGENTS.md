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
| `src/fleet.rs` | worker registry, worker threads, worker events, overseer tool dispatch |
| `src/prompt.rs` | both system prompts, tool schemas, tool-name constants, `DEFAULT_WORKDIR` |
| `src/repos.rs` | git-repository discovery under the configured roots |
| `src/secrets.rs` | `OVERSEER_API_KEY` from the environment or `~/.config/overseer/.env` |
| `src/agent.rs` | `Turn::run`, the model ↔ tools loop shared by overseer and workers |
| `src/tools.rs` | worker tools `bash`, `read_file`, `write_file` |
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
- Quitting calls `tools::kill_running`: without it a worker's `cargo build` outlives the process, because the session never reaps a group that is still running.
- `agent::trim` drops the oldest turns past `HISTORY_BUDGET` before each model call. Without it a long session reaches a context-length error it can never recover from: the failed call leaves history unchanged, so every later turn fails the same way.
- A worker's liveness is its thread's `JoinHandle`, not a bookkeeping counter: a panicked worker cannot leave the fleet busy forever.
- The core never prints: everything visible is a `UiEvent`, and only a frontend touches the terminal. `Event` is the other direction, frontend to core.
- Worker model text streams through `llm::StreamLlm` into `Event::Delta`, which the TUI shows in the worker panel and the core never feeds back to the overseer.
- Worker tool calls emit `Event::Progress` (start, then elapsed); it wakes nothing.
- Debugging a real run: `OVERSEER_DEBUG=1` echoes each model request and every tool result. Prefer `just run`: an interactive shell may hash Homebrew's `overseer` under the same name and silently run the released binary.
- Code, comments, commits and PRs in English.
