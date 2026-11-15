# AI coding harnesses: feature map by scope

Four harnesses, grouped by scope rather than by vendor: **omp**, **jcode**, **opencode**, **hermes**.
Each row asks the same question of all four: is the capability there, and how is it actually implemented.

Checked **2026-11-14**. opencode sources are **V2** docs unless marked V1. jcode published numbers are
**v0.92.14**. Nothing here is from recall: every row was read from official docs, the source repository, or
the vendor's own site during the check.

**Status legend**

| Mark | Meaning |
| --- | --- |
| ✅ | Documented by the vendor, mechanism named |
| 🟡 | Partly documented / documented with a caveat that changes what you can rely on |
| — | Absent from the documentation set I read. Not a claim that the product lacks it |
| ⛔ | Could not be sourced. Refused rather than guessed (see §13) |

`omp://…` citations are documents **shipped with the omp binary**: official and canonical, but with no public
URL. Everything else is a link.

---

## 1. Process and runtime architecture

| Aspect | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Core model | ✅ One process per session (`omp://cli-reference.md`) | ✅ Daemon owns sessions, tools and provider calls; TUI is a socket client ([docs](https://jcode.sh/docs)) | ✅ One shared background server per user; "every local OpenCode client connects to that server, which owns sessions, configuration, integrations, permissions, and tool execution" ([CLI](https://opencode.ai/docs/cli)) | ✅ Gateway process routes sessions, with a documented one-gateway-for-all-profiles mode ([Multiplexing gateway](https://hermes-agent.nousresearch.com/docs/developer-guide/multiplexing-gateway)) |
| Server / attach mode | ✅ `--mode rpc`, `rpc-ui` (line-delimited JSON over stdio), `acp` (Agent Client Protocol) | ✅ `jcode serve`, then clients via `--socket`, including an SSH-forwarded unix socket | ✅ `--server <url>` attaches, `--standalone` runs private, `opencode service set disabled true` turns the shared service off | ✅ Gateway plus three programmatic protocols (ACP, TUI JSON-RPC, OpenAI-compatible HTTP) |
| Remote working directory | ✅ `--cwd`, `--add-dir` | ✅ `-C` local checkout with `--remote-working-dir` absolute path where tools, git and edits run | ✅ `opencode <dir>`; server URL decoupled from checkout | ✅ SSH / Docker / cloud backends per session |
| Language | ✅ TypeScript plus Rust native crates behind `@oh-my-pi/pi-natives` (`omp://user-facing-packages.md`) | ✅ Rust ([jcode.sh](https://jcode.sh/)) | ✅ TypeScript/Bun with per-platform native binaries ([install docs](https://opencode.ai/docs/)) | ✅ Python, embeddable as a library ([Python library](https://hermes-agent.nousresearch.com/docs/guides/python-library)) |
| State store | ✅ Session JSONL per profile; `--session-dir` relocates it | ✅ Sessions and memory held by the daemon | ✅ SQLite; path from `opencode debug paths db` | ✅ `state.db`, with a documented corruption-recovery path ([State DB recovery](https://hermes-agent.nousresearch.com/docs/developer-guide/state-db-recovery)) |
| Profile isolation | ✅ `--profile` scopes auth, sessions, settings, caches; `--alias` creates a shell shortcut | 🟡 Per-session env overrides (`SetSessionEnv`), explicitly ephemeral: lost on daemon restart | 🟡 Config layering global → project → `.opencode`; no documented auth-isolating profile | ✅ Profiles with a sharing mechanism ([Profiles](https://hermes-agent.nousresearch.com/docs/user-guide/profiles), [Profile distributions](https://hermes-agent.nousresearch.com/docs/user-guide/profile-distributions)) |

## 2. Clients and surfaces

| Surface | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| TUI | ✅ Default interface; renderer internals documented (`omp://tui.md`); `omp gallery` previews renderers | ✅ Rust TUI with inline images, LaTeX, mermaid, clickable links ([screenshots](https://jcode.sh/)) | ✅ Full-screen TUI; `opencode mini` for a minimal interface | ✅ Ink TUI, "mouse-friendly, rich overlays" ([TUI guide](https://hermes-agent.nousresearch.com/docs/user-guide/tui)) |
| Headless | ✅ `-p/--print`, `--mode json`, `--mode rpc`, `--print-thoughts`, cost/time caps | ✅ `jcode run "…"`, SDK, auto-poke keeps unfinished runs going ([jcode.sh](https://jcode.sh/)) | ✅ `opencode run "…"`, "designed for scripts, CI jobs" | ✅ Batch trajectory generation with checkpointing ([Batch](https://hermes-agent.nousresearch.com/docs/user-guide/features/batch-processing)) |
| Desktop app | — (browser guest UI instead) | 🟡 Desktop app **beta** | ✅ macOS, Windows, Linux (`.deb`, `.rpm`, AppImage, `.dmg`, `.exe`) | ✅ Hermes Desktop: streaming tool output, side-by-side previews, file browser, voice, cron ([Desktop](https://hermes-agent.nousresearch.com/docs/user-guide/desktop)) |
| Phone / web | ✅ Collab browser guest: streaming transcript, tool cards, subagent views, host interruption; `my.omp.sh` (`omp://user-guide` collab-web) | 🟡 WebSocket gateway for paired thin clients, off by default; `jcode pair` prints a 6-digit code + QR, 5-minute TTL, hashed tokens; iPhone beta | ✅ `opencode pair` prints a single-use link expiring in 5 minutes; Docker image `ghcr.io/anomalyco/opencode:2.0.0` | ✅ 21+ messaging platforms, "19 native to the gateway plus IRC and Microsoft Teams via plugins" ([llms.txt](https://hermes-agent.nousresearch.com/docs/llms.txt)) |
| Editor / ACP | ✅ `omp acp` server over stdio | — | ✅ `/docs/cli/acp` in the V2 index | ✅ ACP adapter with lifecycle, sessions, event bridge, approvals ([ACP internals](https://hermes-agent.nousresearch.com/docs/developer-guide/acp-internals)) |
| Chat platforms | — | — | — | ✅ Telegram, Discord, Slack, WhatsApp, Signal, SMS, email, Matrix, Mattermost, Teams, Feishu, Google Chat, LINE, IRC, ntfy, webhooks ([messaging overview](https://hermes-agent.nousresearch.com/docs/user-guide/messaging)) |

## 3. Providers and authentication

| Capability | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Subscription OAuth | ✅ Provider login; `auth-broker` credential vault and `auth-gateway` forward proxy backed by it | ✅ `claude`, `openai`/Codex, `gemini`, `copilot`, `azure`; headless via `--no-browser`, `--print-auth-url`, `--callback-url`, `--auth-code` | ✅ `/connect` in the TUI; Console OAuth for the managed plane | ✅ Nous Portal OAuth, which also bundles the Tool Gateway |
| Local models | ✅ (`omp://local-models.md`) | ✅ `ollama`, `lmstudio` logins | ✅ Providers list | ✅ Ollama and local LLM guides ([index](https://hermes-agent.nousresearch.com/docs/llms.txt)) |
| Custom OpenAI-compatible endpoint | ✅ `--provider`, `--api-key` (flag docs call `--provider` legacy) | ✅ `jcode provider add` with `--api-key-stdin`, `--set-default`, then `auth-test` | ✅ `providers` config | ✅ Any OpenAI-compatible endpoint, plus Bedrock / Foundry guides |
| Model abstraction | ✅ Roles: `--smol`, `--slow`, `--plan`, `--model <role\|@role\|fuzzy>`; `--models a,b,c` for `Ctrl+P` cycling | ✅ Named provider profiles with failover; an account router falls back across accounts on usage limits | ✅ `provider/model#variant` per agent | ✅ Per-bot and per-profile models; Mixture of Agents presets as selectable models ([MoA](https://hermes-agent.nousresearch.com/docs/user-guide/features/mixture-of-agents)) |
| Key / quota resilience | 🟡 `--service-tier`, `--provider-session-id`, `--prompt-cache-key`; `dry-balance` dry-runs OAuth account balancing | ✅ Failover plus account router | 🟡 Console budgets and billing | ✅ Credential pools with rotation and rate-limit recovery ([Credential pools](https://hermes-agent.nousresearch.com/docs/user-guide/features/credential-pools)), fallback providers |
| Cost control | ✅ `--warn-cost`, `--max-cost` (stop exits **3**), `session.*` settings | 🟡 Budget in ambient mode config | 🟡 Console budgets | 🟡 Billing lifecycle states documented for the TUI ([Billing lifecycle](https://hermes-agent.nousresearch.com/docs/developer-guide/billing-lifecycle)) |

## 4. Coding tools and code intelligence

| Tool class | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Read | ✅ `read` handles line ranges, images, archives, SQLite, PDFs (`omp://tools/read.md`) | ✅ Read tool calls rendered as inline diffs | ✅ `read` tool with its own permission action | ✅ `read_file`, plus document extraction for PDF / Office / notebooks ([Document extraction](https://hermes-agent.nousresearch.com/docs/user-guide/features/document-extraction)) |
| Edit | ✅ `edit` as hashline patches; `write` for whole files | ✅ Edit tool | ✅ `edit`, `write`, `patch` (one shared `edit` action) | ✅ `write_file`, `patch` |
| Search | ✅ `glob`, `grep`, semantic `find` for implementing files and line ranges | ✅ `agentgrep`: "matches include file structure, such as functions and their locations… results are adaptively trimmed based on what the agent has already seen" ([HARNESS.md](https://github.com/1jehuang/jcode/blob/master/docs/HARNESS.md)) | ✅ `glob`, `grep`; the grep permission resource is the regex, not the search path | ✅ Tools grouped into toolsets ([Toolsets reference](https://hermes-agent.nousresearch.com/docs/reference/toolsets-reference)) |
| Structural / AST | ✅ `ast-grep` for patterns, `ast-edit` for codemods; policy prefers them over text edit | — | — | — |
| LSP | ✅ Definitions, references, hover, code actions (`omp://tools/lsp.md`) | ⛔ See §13 | 🟡 V1: 30+ servers, auto-install, disabled by default ([V1 LSP](https://opencode.ai/v1/docs/lsp.md)); V2 says `lsp` is not a current Core permission action | ✅ pyright, gopls, rust-analyzer… wired into the post-write lint check ([LSP](https://hermes-agent.nousresearch.com/docs/user-guide/features/lsp)) |
| Debugger (DAP) | ✅ `debug` tool (`omp://tools/debug.md`) | — | — | — |
| Code execution | ✅ `eval` with persistent Python and Bun/JS kernels, pipes, jobs, tool bridge | 🟡 Shell tool calls, batched in one message | ✅ `execute` action ("Code Mode") | ✅ `execute_code`: Python with RPC tool access ([Code execution](https://hermes-agent.nousresearch.com/docs/user-guide/features/code-execution)) |
| Browser automation | ✅ `browser` over a CDP relay into your own Chrome; `omp browser-relay install` | 🟡 `webfetch`; `websearch` engines (DuckDuckGo, Bing, SearXNG, Kagi) with per-session proxies | 🟡 `webfetch`, `websearch` actions | ✅ `browser_exec`: Hermes Chromium, cloud browsers, Lightpanda, or your own Chrome via CDP ([Browser](https://hermes-agent.nousresearch.com/docs/user-guide/features/browser)) |
| Desktop automation | ✅ `computer` (windows, screenshots, AX tree, native input) (`omp://tools/computer.md`) | — | — | ✅ Computer use ([Computer use](https://hermes-agent.nousresearch.com/docs/user-guide/features/computer-use)) |
| Binary / reverse engineering | ✅ `ida` (`omp://tools/ida.md`) | — | — | — |
| Media generation | ✅ `generate_image`, `tts` | ✅ Native image generation (site screenshot) | 🟡 `experimental_image_generate` permission action exists | ✅ Image generation via FAL ([Image generation](https://hermes-agent.nousresearch.com/docs/user-guide/features/image-generation)), TTS, vision |

## 5. Execution, isolation, approval

| Concern | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Approval model | ✅ Tiers `always-ask`, `always-ask-cached`, `write`, `yolo`; `--yolo` still honours explicit policies and provider safety checks (`omp://approval-mode.md`) | 🟡 Two tiers (auto-allowed vs `request_permission`) in a doc marked **`Status: Design`**, "currently the only consumer is ambient mode" ([SAFETY_SYSTEM.md](https://github.com/1jehuang/jcode/blob/master/docs/SAFETY_SYSTEM.md)) | ✅ Rule list `{action, resource, effect}`, last match wins, no match ⇒ `ask`; any `deny` denies a multi-resource op; approvals `once` / `always` / `reject`; saved approvals are project-scoped allow rules that never override a `deny` ([Permissions](https://opencode.ai/docs/permissions)) | ✅ Dangerous-command approval, user authorization, container isolation ([Security](https://hermes-agent.nousresearch.com/docs/user-guide/security)) |
| Shell analysis | — | — | ✅ Built-in scanner across Bash, Zsh, POSIX `sh`, PowerShell; unanalysable syntax errors instead of executing unverified subcommands | — |
| Org-level policy | — | — | ✅ Two policy plugins "ignore removals so that a repository cannot switch off policy enforcement" ([Plugins](https://opencode.ai/docs/plugins)) | ✅ Managed scope: administrator-pinned, user-immutable config and secrets ([Managed scope](https://hermes-agent.nousresearch.com/docs/user-guide/managed-scope)) |
| Sandbox / isolation backend | 🟡 `--no-pty`, `--no-lsp`, `--tools`, `--no-tools`, `--trusted-extension`; no container backend documented | ✅ Explicitly *not* a sandbox: `SetSessionEnv` "is not a security sandbox or secret-redaction system" ([HARNESS.md](https://github.com/1jehuang/jcode/blob/master/docs/HARNESS.md)) | 🟡 Permissions warn "`shell` runs with the host user's filesystem, process, and network authority" | ✅ Local, Docker, SSH, Daytona, Modal, Vercel Sandbox, plus plugin backends such as Singularity; Daytona/Modal hibernate when idle |
| Network egress | — | 🟡 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` per session for `webfetch` and `websearch`; invalid settings fail closed with a redacted error | — | ✅ Egress proxy, iron-proxy credential injection, Docker network allowlisting ([Egress](https://hermes-agent.nousresearch.com/docs/user-guide/egress)) |
| Secrets | ✅ Never inline in MCP config: header resolver reads a SOPS bundle at connect time (`omp://secrets.md`) | ✅ `--api-key-stdin` avoids shell history; per-provider `.env` files | 🟡 Env/config | ✅ Secret sources incl. 1Password, Bitwarden Secrets Manager, command helper; credential vault so the agent signs in without seeing passwords ([Credential vault](https://hermes-agent.nousresearch.com/docs/user-guide/features/credential-vault)) |
| Run bounds | ✅ `--max-time`, `--max-cost`, `--warn-cost` | 🟡 Ambient cycle caps; unrecoverable failures stop the auto-poke loop | ✅ `steps` per agent | 🟡 Cron + goals; no spend ceiling documented |

## 6. Context engineering

| Mechanism | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Instruction files | ✅ AGENTS.md, CLAUDE.md, rules pipeline; `--no-rules`, `--no-context-files` (`omp://context-files.md`) | ✅ `AGENTS.md` at repo root and `~/AGENTS.md` machine-wide, both loaded per session in scope | ✅ Instructions sources documented separately from skills and references | ✅ `.hermes.md`, AGENTS.md, CLAUDE.md, `.cursorrules`, global `SOUL.md` ([Context files](https://hermes-agent.nousresearch.com/docs/user-guide/features/context-files)) |
| Skills | ✅ `skill://name` URIs, `--skills "git-*,docker"` filtering, marketplace installs (`omp://skills.md`) | ✅ Conversation embedded as a vector; a skill is injected when the conversation matches, same mechanism as memory recall; Claude Code plugin skills load unchanged | ✅ `skill` permission action; a skill can propose its own saved allow pattern | ✅ Progressive disclosure, agent-managed, agentskills.io compatible, Skills Hub; ~90 bundled + ~60 optional |
| Skill maintenance | — | — | — | ✅ Curator: usage tracking, staleness, archival, LLM-driven review ([Curator](https://hermes-agent.nousresearch.com/docs/user-guide/features/curator)) |
| Memory | ✅ `mnemopi` backend: store/recall/update/delete, `sleep`/`consolidate`, JSON export/import, scratchpad, banks (`omp://mnemosyne-memory-backend.md`) | ✅ `[features] memory = true`, memory sidecar under `[agents]` | ⛔ See §13 | ✅ MEMORY.md + USER.md + session search; pluggable providers Honcho, Mem0, Supermemory, RetainDB, Hindsight, ByteRover, OpenViking, Holographic |
| Compaction | ✅ Session compaction plus **SnapCompact**: deterministic local serialization and PNG rendering of discarded history for vision-model compaction, no model call or key (`omp://user-facing-packages.md`) | ✅ Per-session compaction ceiling; "the effective budget is the smaller of the model's context window and the session ceiling"; overrides saved with the session and restored on resume | ✅ Auto by default: summary + `keep.tokens` (15000) + `buffer` share of the window; optional provider-native compaction returning an opaque checkpoint; experimental background compaction ([Compaction](https://opencode.ai/docs/compaction)) | ✅ ContextCompressor as a replaceable plugin seam; micro-compaction amortises compression cost ([Micro-compaction](https://hermes-agent.nousresearch.com/docs/developer-guide/micro-compaction)) |
| Token optimisation | 🟡 Model roles per task class; `smol` handoff via prewalk | ✅ `agentgrep` trimming; KV cache monitor classifying cache misses; "cache cold" notification in the TUI | ✅ Compaction buffer reuse of the prompt cache | ✅ Prompt assembly keeps cache stability; micro-compaction trading cached-prefix reuse ([Prompt assembly](https://hermes-agent.nousresearch.com/docs/developer-guide/prompt-assembly)) |
| Staying on task | ✅ `todo` tool; **prewalk** one-shot handoff at the first eligible edit turn, gated on a successful todo call (`omp://prewalk.md`); goal and plan-yolo modes | ✅ Phases → lanes → items with per-item `confidence`; `waiting_on` / `requires_user_auth` pause one todo, not the lane; auto-poke; hill-climbability rating on goals | ✅ `steps` cap; final step removes tools and forces a summary; hidden compaction/title/summary agents | ✅ Persistent Goals ("our take on the Ralph loop"), session heartbeats, recurring loops |
| Persona | ✅ System-prompt override, Handlebars template, or append (`omp://system-prompt-customization.md`) | ✅ `~/.jcode/swarm-prompt.md` plus per-project override | ✅ Per-agent `system` prompt | ✅ Global `SOUL.md`, built-in personalities, pets/mascots |

## 7. Multi-agent and unattended work

| Capability | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Subagents | ✅ `task` with agent type per task, optional isolated worktree, results written back to the live session (`omp://tools/task.md`) | ✅ Swarm tool: agents spawn their own workers, "turning the main agent into a coordinator and the swarm agents into workers" ([README](https://github.com/1jehuang/jcode)) | ✅ Child sessions with fresh context, always background; parent gets a report when a child goes idle and after later rounds | ✅ `delegate_task` spawns isolated children ([Delegation](https://hermes-agent.nousresearch.com/docs/user-guide/features/delegation)) |
| Cross-agent messaging | ✅ `agent://<id>` write delivers a message, `agent://all` broadcasts; `history://<id>` reads a concise transcript (`omp://eval/agents`) | ✅ DM one agent, broadcast to all agents on the server, or scope to the repo | — (parent/child reports only) | 🟡 Bots message each other through group chats and `@mentions` ([Bot mode](https://hermes-agent.nousresearch.com/docs/user-guide/bot-mode)) |
| Conflict handling | ✅ Worktree isolation per task; one integration owner rule | ✅ File-change notice to any agent that read a file another agent edited; agent may ignore or diff | 🟡 Separate sessions; no conflict notice documented | ✅ Git worktrees for several agents on one repo ([Git worktrees](https://hermes-agent.nousresearch.com/docs/user-guide/git-worktrees)) |
| Agent definitions | ✅ Bundled task agents, `omp agents` management (`omp://task-agent-discovery.md`) | ✅ Config-driven swarm routing and concurrency cap | ✅ Markdown + frontmatter or JSONC; modes `primary` / `subagent` / `all`; per-agent model, permissions, step cap | ✅ Bot roster: own model, memory, skills, routines, chats |
| Long-running / scheduled | ✅ Async `bash` jobs, `proc://` supervision, `--wait-async` in print mode | ✅ Ambient mode with schedule, budget, work branches | 🟡 Server keeps sessions alive; no scheduler | ✅ Cron in natural language delivered to any platform; Kanban SQLite board across profiles |
| Code-mode tool calling | 🟡 `eval` kernels call tools through the bridge | — | ✅ Code Mode, removable by denying `execute` | ✅ `execute_code` collapses multi-step pipelines into one inference call |

## 8. Extensibility and interop

| Surface | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| MCP | ✅ Client with several transports and documented runtime lifecycle (`omp://mcp-protocol-transports.md`) | 🟡 "Only stdio servers are supported today. HTTP and SSE entries are recognized and skipped"; imports Claude Code and Codex servers on first run | ✅ MCP servers page in V2; each MCP tool becomes a `<server>_<tool>` permission action | ✅ MCP connect plus control over exactly which tools load ([MCP](https://hermes-agent.nousresearch.com/docs/user-guide/features/mcp)) |
| Plugin surface | ✅ Extensions and hooks with UI (`ctx.hasUI`, extension dialogs), trust allowlist `--trusted-extension`, marketplace (`omp://extension-loading.md`) | 🟡 Hooks only: shell commands at turn, session and tool boundaries | ✅ npm / git / local dirs; server, TUI-only and desktop-GUI plugins; enable-disable patterns (`*`, `-id`); hot reload; `opencode plugin add\|list\|check\|update\|remove` | ✅ Tools, hooks, middleware, observer hooks, per-subsystem providers; documented public import surface ([Public plugin API](https://hermes-agent.nousresearch.com/docs/developer-guide/plugins/public-api)) |
| Plugin distribution | ✅ `omp plugin` manage, marketplace, `--plugin-dir` | — | ✅ npm names, tags, ranges, git shortcuts, SSH, `::path:` subdirectories; `plugin check` / `update` | ✅ Reviewed catalog installable in one click ([Plugin catalog](https://hermes-agent.nousresearch.com/docs/user-guide/features/plugin-catalog)) |
| Programmatic API | ✅ RPC, ACP, `sdk.md` (`omp://sdk.md`) | ✅ TypeScript SDK; `jcode api-bridge` attaches to a running daemon, works over forwarded sockets ([SDK](https://jcode.sh/sdk)) | ✅ Server API, JS client, SDK, Effect variants | ✅ ACP, TUI gateway JSON-RPC, OpenAI-compatible HTTP server ([API server](https://hermes-agent.nousresearch.com/docs/user-guide/features/api-server)) |
| Custom commands | ✅ Slash-command internals plus custom tools (`omp://slash-command-internals.md`) | ✅ `/skillname` and command set | ✅ Commands page in V2 | ✅ Slash commands per platform, with gateway parity documented |
| Import from other agents | ✅ `--from-claude`, `--from-codex` | ✅ Reads `~/.claude.json`, `.mcp.json` | — | ✅ One command imports OpenClaw, Claude Code, Codex CLI, Cursor, Grok Bot, OpenBot: instructions, rules, allowlists, MCP, skills, memories, routines |

## 9. Sessions, rollback, self-modification

| Capability | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Resume | ✅ `-c`, `-r` picker, and `--restore` which continues "what its last process left unfinished when it crashed or was killed: the interrupted turn and the subagents it was running" | ✅ `--resume <name>` picker with previews; survives a dropped SSH because the daemon keeps working | ✅ Server-side sessions; `/undo` restores the prompt into the composer | ✅ Persistence, resume, search, per-platform tracking ([Sessions](https://hermes-agent.nousresearch.com/docs/user-guide/sessions)) |
| Fork / handoff | ✅ `--fork`, `--export <session>` to HTML | ✅ `/fork` into a new window; `/transfer` compacts into a fresh handoff session | ✅ Fork, with a one-time fork notice that can be disabled | 🟡 Batch checkpointing; no fork command found |
| Undo of file changes | ✅ `checkpoint` / `rewind` tools (`omp://tools/rewind.md`) | ✅ `/rewind` | ✅ Snapshots per model step, scoped to the session's active directory in a git worktree; separate internal git object DB, "does not create commits, move branches, or intentionally change your repository's index" ([Snapshots](https://opencode.ai/docs/snapshots)) | 🟡 Checkpoints **opt-in** ("default is off"); shadow repo at `~/.hermes/checkpoints/store/`; `/rollback <N>` keeps hand edits via an agent-write ledger, `--all` overrides; no checkpoints on container backends |
| Session search | ✅ `history://`, `archive` (recent sessions, recaps, prompt history) | ✅ Resume menu with previews | ✅ SQLite-backed | ✅ FTS5 cross-session recall |
| Self-modification | — | ✅ Self-dev: "it will start modifying its own source code… edit, build, and test its own source code, then reload its own binary and continue work in your (potentially many) sessions" | — | — |
| Sharing / recording | ✅ `/record` → `omp clip` public URL; collab link (`omp://collab.md`) | 🟡 Paired thin clients speak the same protocol as the TUI | ✅ Web app, share | ✅ Trajectory export aimed at RL training ([Trajectory format](https://hermes-agent.nousresearch.com/docs/developer-guide/trajectory-format)) |

## 10. Automation, observability, benchmarking

| Capability | omp | jcode | opencode | hermes |
| --- | --- | --- | --- | --- |
| Self-hosted GitHub bot | ✅ **robomp**: webhook triage, "resumes an `omp --mode rpc` session per issue, comments or opens a fix PR", operator dashboard, SQLite queue (`omp://user-facing-packages.md`) | — | — | ✅ Webhook-triggered runs for GitHub/GitLab, plus a documented PR-review agent recipe ([Webhooks](https://hermes-agent.nousresearch.com/docs/user-guide/messaging/webhooks), [PR review guide](https://hermes-agent.nousresearch.com/docs/guides/github-pr-review-agent)) |
| Usage analytics | ✅ `omp stats` dashboard: request/token/cost, provider, model, folder, tool, frustration; SSE; `omp-stats` standalone | 🟡 `/usage` usage limits; cache-cold notification | 🟡 Console analytics/budgets | ✅ Web dashboard: logs, analytics, sessions, cron, memory ([Web dashboard](https://hermes-agent.nousresearch.com/docs/user-guide/features/web-dashboard)) |
| Benchmarking | ✅ `metaharness` (Harbor/Terminal-Bench, TS-edit, SnapCompact), `bench`, `if-bench`, `cleanse` | ✅ Own bench program: continuous optimisation score, "uncontaminatable by construction"; confidence-stepping study | — | ✅ Batch trajectories for RL; Atropos training mentioned on the product page |
| Quality feedback loop | ✅ `omp grievances` collects reported tool issues from auto-QA | ✅ Auto-poke retries transient errors, stops on unrecoverable ones | — | 🟡 Curator reviews agent-written skills |
| Notifications | 🟡 Session-level, TUI | ✅ Turn-complete notifications and sounds; safety review via email/SMS/desktop/webhook (design doc) | — | ✅ Delivery to any messaging platform |

## 11. Distinctive extras

| Harness | Not found elsewhere in this set |
| --- | --- |
| omp | LSP + DAP + AST tools in one loop; browser relay into your own Chrome; desktop `computer`; JS/Python kernels as tools; `ida`; `omp git` fullscreen UI; `compress` rewriting text into a dense prompt register; advisor/watchdog passive review per turn; prewalk handoff; SnapCompact bitmap compaction; metaharness; robomp |
| jcode | Self-dev with binary hot-reload; swarm file-conflict notice and messaging channels; per-todo confidence with forced re-check on spikes; hill-climbability gate on goals; auto-poke; account router; sub-10 MB extra sessions |
| opencode | Permission DSL over action × resource with scanner-backed shell analysis; non-removable policy plugins; three plugin runtimes (server, TUI, desktop GUI); shared-server model with `--standalone` escape hatch |
| hermes | 21+ chat platforms; agent-written and self-improving skills with a Curator; six terminal backends incl. serverless hibernation; voice mode in chat; `SOUL.md`; credential vault; Bot Mode roster; Kanban board |

## 12. Where the harnesses actually differ

- **Overlap is the loop.** Read, edit, search, shell, instruction file, MCP, subagents, headless mode: all four have it. Differentiation lives outside it.
- **Context strategy is the fork.** omp spends context on tools and protocols. hermes spends it on persistence (memory, skills, cron, chat reach). jcode spends it on throughput (daemon, swarm, cheap sessions). opencode spends it on governance (one server, permission DSL, plugins).
- **Pick by failure mode.** Tool reach too small → omp. Agents forget or throttle → hermes. Hundreds of parallel agents on one box → jcode. Org needs auditable permissions and one shared state → opencode.

## 13. Refusals and conflicting sources

**Refused: searched, not sourceable**

1. **jcode LSP and jcode sandboxing.** Searched `site:jcode.sh LSP language server` and `jcode.sh docs sandbox isolation permission`; read the docs page, `docs/HARNESS.md`, `docs/SAFETY_SYSTEM.md`. HARNESS.md documents an MCP client and per-session proxies, no LSP. Next: `grep -ri lsp crates/` in the repo, or ask the maintainer.
2. **opencode memory.** No memory page in the V2 docs index. Persisted state is documented for sessions, snapshots and compaction only. Next: read `packages/opencode/src/session` in the repository.
3. **opencode LSP in V2.** V1 is explicit; V2's permissions page says `lsp` "is not [a] current V2 Core permission action", implying a change but not whether language servers remain.
4. **omp public documentation.** `omp://` is the shipped doc set. No public URL exists to cite; license, repository and release channel are unchecked.
5. **Why omp's standalone memory CLI defaults to `~/.hermes/mnemopi/data/mnemopi.db`.** Unexplained in the docs; not guessed at.

**Conflicting official sources**

- **jcode memory per session.** README: first session 71.3 MB, ~3.3 MB per additional. Site: 3.5 MB extra headless, 10.4 MB extra interactive, "measured: jcode v0.92.14". Same vendor, different builds and methods; the headline ratio is the most favourable pairing. Treat all such figures as vendor-measured.
- **hermes platform count.** "20+ platforms" on the product page vs "21+ … 19 native plus IRC and Microsoft Teams via plugins" in llms.txt; neither matches the messaging overview's enumeration, which adds LINE, Raft, Webhooks, BlueBubbles. The plugin split in llms.txt is the more precise reading.

**Rendering conventions** — "None documented" / "—" means absent from the documentation set read for that harness, never a claim about the product. 🟡 marks a vendor caveat that changes what you can rely on. ⛔ marks the five items above.

---

## Sources

- omp: `omp://` documents shipped with the binary (`cli-reference`, `user-facing-packages`, `approval-mode`, `secrets`, `skills`, `marketplace`, `context-files`, `prewalk`, `compaction`, `extension-loading`, `mcp-protocol-transports`, `sdk`, `collab`, `system-prompt-customization`, `task-agent-discovery`, `tools/*`).
- jcode: <https://jcode.sh/docs>, <https://jcode.sh/>, <https://jcode.sh/sdk>, [github.com/1jehuang/jcode](https://github.com/1jehuang/jcode) (`README.md`, `docs/HARNESS.md`, `docs/SAFETY_SYSTEM.md`).
- opencode: <https://opencode.ai/docs/> (V2) and the V2 index <https://opencode.ai/docs/llms.txt>; V1 LSP page for the deprecated behaviour.
- hermes: <https://hermes-agent.nousresearch.com/docs> and its `llms.txt` index; product page.

## Others (verified this session)

- **Claude Code**: one engine across terminal, VS Code, JetBrains, desktop, web and mobile; CLAUDE.md plus auto memory; skills; hooks; subagents and background agents; Agent SDK; `-p` piping; cloud and desktop scheduled tasks; `/loop`; Remote Control; Slack; Chrome. ([Overview](https://code.claude.com/docs/en/overview))
- **Codex CLI**: local agent from OpenAI; ChatGPT plan or API key; separate IDE extension, `codex app` desktop and Codex Web cloud; Apache-2.0. Sandbox and approval modes are *not* in the README, so the earlier claim about them is withdrawn. ([README](https://github.com/openai/codex))
- **Aider**: repo map; git auto-commit; lint/test loop after every change; voice-to-code; images and web pages in chat; IDE watch mode; copy/paste bridge to web chat; 100+ languages. Its README still names Claude 3.7 Sonnet / GPT-4o-era models as "works best", so the page lags current lineups. ([README](https://github.com/Aider-AI/aider))
- **Gemini CLI**: omitted. Not read this session; not asserted.
