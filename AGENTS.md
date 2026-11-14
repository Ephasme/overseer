# Agent Instructions

## Critical constraints

- The overseer agent never does work itself: `fleet::overseer_tool_definitions` holds only fleet tools. Never give it `bash`, `read_file`, `write_file` or any other tool that touches files or runs commands; `list_repos`, which only names directories under the configured roots, is the one read the overseer gets.
- Each worker's repository must pass `config::resolve_repo`: an existing directory under `roots` in `~/.config/overseer/config.toml`. Never add a tool that clones a repository. This picks the worker's starting repository only; workers are not sandboxed, so `bash` and absolute paths reach the whole filesystem.
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
| `src/main.rs` | event loop: stdin and worker events drive overseer turns |
| `src/fleet.rs` | worker threads, worker events, the overseer's tools and prompt |
| `src/agent.rs` | `run_turn`, the model ↔ tools loop shared by overseer and workers |
| `src/tools.rs` | worker tools `bash`, `read_file`, `write_file` |
| `src/llm.rs` | chat-completions wire types, HTTP client, `testing::Scripted` model double |
| `src/config.rs` | `config.toml`, roots, `resolve_repo`, `~/.config/overseer/.env`, state directory |
| `.github/workflows/ci.yml` | checks; on a `v*` tag: build, release, Homebrew formula update |
| `.github/scripts/update-formula.sh` | rewrites the tap's formula from a release |
| `justfile` | `just install`, `just run`, `just check` |

## Key conventions

- Handle errors with `Result`; outside tests, `unwrap`, `panic!`, slice indexing and `as` casts fail the lints.
- A tool failure goes back to the model as text starting `error: `; it never aborts a turn.
- Tests run offline: script the model with `llm::testing::Scripted`, never call a real endpoint. nextest kills a test after 60 s (`.config/nextest.toml`).
- Tool arguments are typed structs with `deny_unknown_fields`: a wrong key or a missing field comes back to the model as `error: invalid arguments: …`, naming the allowed fields.
- A worker's `bash` runs in its own process group and is killed after 300 s (`tools::BASH_TIMEOUT`): without the group kill, a surviving grandchild holds the pipes and the tool call never returns.
- Worker tool calls emit `Event::Progress` (start, then elapsed); `main` prints those to stderr and does not wake the overseer model with them.
- Debugging a real run: `OVERSEER_DEBUG=1` echoes each model request and every tool result. Prefer `just run`: an interactive shell may hash Homebrew's `overseer` under the same name and silently run the released binary.
- Code, comments, commits and PRs in English.
