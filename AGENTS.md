# Agent Instructions

## Critical constraints

- In a git repository, an agent never works in the primary checkout: every change goes through a git worktree created from the base branch, and the primary checkout holds no work in progress — no modified files, no stashes, no in-flight rebases.
- Every agent is the same implementation with the same tools: `swarm::AgentRun` runs one turn for the root and for the deepest child alike, and `prompt::agent_tool_definitions` is the only tool set. An agent may `spawn_agent`, `message_agent`, `control_agent` and `list_agents` only for agents strictly below it in the tree (`Swarm::owns`); the root is agent 1, the one the user talks to.
- An agent's `Policy` is merged once, before its thread starts, and never changes: there is no tool that restricts a running agent, and `control_agent` has no `restart`. Changing rights means stopping an agent (which stops its whole subtree) and spawning a new one.
- `max_depth` in `config.toml` counts delegations below the root and defaults to 3; a spawn past it is refused, never clamped.
- Each agent starts in an existing directory: `config::resolve_dir` canonicalises it and rejects a non-directory. `roots` in `~/.config/overseer/config.toml` bounds only repository discovery (`list_repos`); it is not a sandbox. Never add a tool that clones a repository. Agents are not sandboxed, so `bash` and absolute paths reach the whole filesystem.
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
| `src/main.rs` | picks the frontend, creates both channels, connects the MCP servers, starts the session thread |
| `src/session.rs` | the router: user input to the root, agent events to their owner and the frontends |
| `src/event.rs` | `UiEvent`, everything the core asks a frontend to show |
| `src/plain.rs` | line-mode frontend: stdin lines in, plain lines out, and `-p`/`--print` / `--mode json` |
| `src/tui/mod.rs` | terminal lifecycle and its panic hook, the event loop, key bindings, the mouse capture F5 hands back |
| `src/tui/app.rs` | transcript entries, composer, agent panel, status line, wrapping |
| `src/tui/theme.rs` | the palette in three variants (dark, light, mono) and the `NO_COLOR` rule behind `theme = "auto"` |
| `src/swarm.rs` | the agent tree: the shared `Swarm`, per-agent threads, ownership, depth, tool dispatch |
| `src/names.rs` | agent codenames: two word lists mixed from the id, and the `name (id)` label |
| `src/prompt.rs` | the one system prompt, the tool schemas, the tool-name constants |
| `src/repos.rs` | git-repository discovery under the configured roots |
| `src/secrets.rs` | `OVERSEER_API_KEY` from the environment or `~/.config/overseer/.env` |
| `src/agent.rs` | `Turn::run`, the model ↔ tools loop every agent runs |
| `src/tools.rs` | the file tools `bash`, `read_file`, `write_file`, `edit`, and the `Policy` that guards them |
| `src/diff.rs` | Myers over interned line ids, and the compact `-N|…` / `+N|…` rows both the model and the transcript read |
| `src/edit.rs` | the line-anchored `edit` patch engine and the per-agent `Store` of files read |
| `src/skills.rs` | the skill catalogue discovered from the user's and the project's roots, read through `skill://` |
| `src/slash.rs` | the slash-command table the composer's completion popup reads |
| `src/mcp/` | MCP servers: config discovery, stdio and HTTP clients, OAuth, the registry an agent calls `mcp__` tools through |
| `src/profiles.rs` | agent profiles discovered from `~/.overseer/agent/profiles`, the built-in profiles, the `Role` that narrows one, and the text `profile://` reads |
| `src/pipelines.rs` | the Rhai pipeline catalogue, the `//! requires:` parser, the hardened engine, the host SDK and the bounded runner |
| `src/conflict.rs` | the conflict registry: numbered merge-conflict blocks an agent resolves by `conflict://<N>` |
| `src/artifacts.rs` | tool output too large for one result, kept per process and read back by `artifact://` |
| `src/jobs.rs` | background `bash` commands, their pipes and process groups, behind `proc://` |
| `src/memory.rs` | per-project notes under the state directory, behind `memory://` |
| `src/vault.rs` | the `.env` credential file, behind `vault://` |
| `src/rules.rs` | the instruction files that govern a directory, behind `rule://` |
| `src/forge.rs` | `gh` and `glab` invocations, behind `issue://`, `pr://` and `mr://` |
| `src/url.rs` | internal URIs: the scheme table, `/schemes`, and what a `scheme://…` path resolves to |
| `src/llm.rs` | chat-completions wire types, SSE streaming, `StreamLlm`, `testing::Scripted` |
| `src/config.rs` | `config.toml`, roots, `max_depth`, the root's `dir`, `home`, `resolve_dir`, state directory, `io_error` |
| `.github/workflows/ci.yml` | checks; on a `v*` tag: build, release, Homebrew formula update |
| `.github/scripts/update-formula.sh` | rewrites the tap's formula from a release |
| `justfile` | `just install`, `just run`, `just check` |

## Key conventions

- Handle errors with `Result`; outside tests, `unwrap`, `expect`, `panic!`, slice indexing and `as` casts fail the lints.
- A tool failure goes back to the model as text starting `error: `; it never aborts a turn.
- `edit` refuses a file its agent has not read, or one whose hash changed since: `read_file` records the whole file's hash, its line count and the range it showed, and `write_file` and `edit` record what they wrote. The store is per agent, so a read by one worker licenses nothing for another.
- Tests run offline: script the model with `llm::testing::Scripted`, never call a real endpoint. nextest kills a test after 60 s (`.config/nextest.toml`).
- Tool arguments are typed structs with `deny_unknown_fields`: a wrong key or a missing field comes back to the model as `error: invalid arguments: …`, naming the allowed fields.
- An agent's `bash` runs in its own process group and is killed after 300 s (`tools::BASH_TIMEOUT`): without the group kill, a surviving grandchild holds the pipes and the tool call never returns.
- An owner reaches an agent through `swarm::Control`: pause holds the inbox, stop drops it, urgent jumps the queue and sets the flag `agent::Turn` checks before every model call and every tool call. A paused agent is not busy, so ctrl+D stops waiting on it.
- `Control::set_policy` has two callers, `start_root` and `Swarm::spawn`, each running it before its thread exists. The policy is then fixed: it is still re-read on every tool call and rewritten into the prompt each turn, but nothing in the program changes it afterwards.
- The root always runs the built-in `supervisor` profile and `Role::Supervisor`; a `~/.overseer/agent/profiles/supervisor.md` replaces its text, model, effort and tool list, never its role or its read-only restriction.
- A profile's `allow_tools` is the whole right: tools are granted one by one, `mcp__<server>` grants one server's tools, and an empty list leaves an agent able to think and answer only. No tool changes a policy after the agent starts.
- A child gets the intersection of the two tool lists. `free_spawn` is a grant, not a callable tool: an owner that holds it may pass `spawn_agent(free: true)`, and the child then gets its profile's policy. The built-in supervisor grants `free_spawn`, so the read-only root can start full-rights children; a custom `supervisor.md` without it is strict.
- `spawn_agent` requires a profile, so every agent's rights have one place to be read; `worker` is the generalist (code, configuration, commands).
- Pipelines are `~/.overseer/pipelines/<name>.rhai`, loaded once at startup like profiles; `/pipeline <name>` or `start_pipeline` runs one as a child agent in the caller's directory, and it stops with the session. Each file opens with `//! requires: <tool>[, <tool>...]`, and every run is bounded by an operation count, a wall clock and the engine's data sizes.
- `close` sends an agent's final report and closes it; the owner re-opens it with an instruction, and only the owner archives it. An archived agent leaves the user's default view; its entry, id and log stay.
- A conflict id belongs to one agent's `conflict::Registry`, not the process: ids never collide across agents, and a resolved id stays dead so a stale write fails loudly. A resolution splices out the marker lines only, keeping the code around them.
- A `scheme://…` path is resolved by `url::SCHEMES`, never by a tool: a scheme exists by having a row there, and its line then reaches the tool descriptions and `/schemes` on its own. Every row resolves, so an unknown name is refused with the full list of the ones that do.
- `slash::COMMANDS` is the one list of slash commands, and `swarm::Event::from_line` is still the one parser: a test submits every row of the table through it, so a command added to one side without the other fails the build instead of reaching the model as a prompt.
- Quitting calls `tools::kill_running` and `Registry::kill_all`: without them an agent's `cargo build`, a background `bash` job or an MCP server outlives the process.
- An agent's liveness is its thread's `JoinHandle`, not a bookkeeping counter: a panicked agent cannot leave the swarm busy forever.
- The core never prints: everything visible is a `UiEvent`, and only a frontend touches the terminal. `Event` is the other direction, frontend to core.
- A model call reaches a view one way: `llm::StreamLlm` turns its pieces into `Event::Thought` and `Event::Delta`, and each becomes one entry appended to that agent's own `Stream`, in the order the call produced them. `Event::Instruction` opens a worker's stream, `Event::TurnStart` breaks it into paragraphs, and `Event::Progress` adds the tool card; nothing rewrites an entry already written, and a finished turn whose answer streamed is not written again. The core never feeds any of it back up the tree.
- A `UiEvent` carrying a non-root agent id goes to that agent's `Stream` only; the lead's stream (`App::lead`) gets one lifecycle line per agent event and never an agent's tool call or message. The root's own events go straight into the lead's stream. `Stream` holds the entries, the flatten cache and the scroll position, so switching views loses neither buffer nor position. F3 is the one verbosity flag: it re-flows every stream.
- An agent's view owns the keys while it is open (`tui::app::App::worker_key`): `esc` stops that agent and its subtree while it has work, leaves the view otherwise, and never quits; no key typed there reaches the root's prompt. The stop travels as `Event::Stop` into `Swarm::stop`, which the root refuses, so no key can end the user's only voice. Quitting stays on ctrl+c and ctrl+d.
- Tool calls emit `Event::Progress` (start, then elapsed) for every agent, the root's included; it wakes nothing.
- Debugging a real run: `OVERSEER_DEBUG=1` echoes each model request and every tool result. Prefer `just run`: an interactive shell may hash Homebrew's `overseer` under the same name and silently run the released binary.
- Code, comments, commits and PRs in English.
