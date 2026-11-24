# 1. v0 architecture: Rust agent server, React client, SSE, SeaORM

- Status: Accepted
- Date: 2026-11-24

## Context

overseer is a coding agent harness built from scratch. v0 must stream model output, run tools on the local machine, survive a server restart without losing sessions, and be driven from a web UI rather than a terminal.

## Decision

### Process split

One Rust server (tokio, axum) owns all state and runs the agent loop. A React + TypeScript web app (Vite) only displays state and sends commands. A turn keeps running when no client is connected.

The agent loop lives in a crate with no HTTP or database dependency and emits events through a trait. Persistence and transport plug in behind it.

### Session actor

Each live session is owned by one tokio task, the only writer of its events. Every command goes through it: new message, cancel, SSE subscription. It holds:

- the next `seq`, loaded once from `max(seq)` when the actor starts;
- the turn state, so a message during a running turn is refused with 409;
- the partial assistant text of the reply being streamed;
- the session's `broadcast` sender.

No lock and no database sequence orders events; the `(session_id, seq)` primary key catches a violation.

### Transport: REST for commands, SSE for events

All routes live under `/api`, so they never collide with client routes.

```
POST /api/sessions                 {cwd}   -> {id}
GET  /api/sessions                         -> [{id, cwd, title, status, updated_at}]
GET  /api/sessions/:id/events?after=N      -> text/event-stream
POST /api/sessions/:id/messages    {text}  -> 202 | 409 while a turn runs
POST /api/sessions/:id/cancel      {}      -> 202
```

Each durable event goes out with `id: <seq>`; ephemeral events carry no `id`, so `EventSource`'s last event ID stays the last durable `seq`. A reconnecting client resumes from `Last-Event-ID` (sent by `EventSource` itself) or `?after=`.

Connecting, in this order, so no event falls between replay and live stream:

1. Ask the actor for `(receiver, partial_text, last_seq)` in one message: subscription and snapshot are taken at the same instant.
2. Send durable events from the database with `cursor < seq ≤ last_seq`.
3. If a turn is running, send `partial_text` as one `text_delta`.
4. Forward live events, dropping any durable event with `seq ≤ last_seq`.

The stream starts with `retry: 500` and sends a `: ping` comment every 15 s. A receiver that lags behind the broadcast buffer has its connection closed; the client reconnects and catches up through replay.

Protocol types are defined once in Rust; the client's types and validators are generated from them ([ADR 0002](0002-toolchain-and-frontend-stack.md)).

### Event log as the single source of truth

Per session, an append-only log. The model's message history and the UI's view are both rebuilt from it.

- **Durable** (stored, carry `seq`): `user_message`, `assistant_message {text, reasoning?, tool_calls, interrupted}`, `tool_result {call_id, ok, output}`, `turn_end {reason}`, `error`.
- **Ephemeral** (broadcast only): `text_delta`, `reasoning_delta`, `tool_call_delta`, `tool_progress`.

A durable event is broadcast only after its write commits.

Every tool call in an `assistant_message` gets a `tool_result`, because the API rejects a history where one is missing:

- **Cancel during streaming**: the partial text is stored as `assistant_message {interrupted: true, tool_calls: []}`; tool calls not yet fully received are dropped.
- **Cancel between tool calls**: each call not yet run gets `tool_result {ok: false, output: "cancelled before running"}`.
- **Cancel during a tool call**: its process group is killed and it gets `tool_result {ok: false}` with the output captured so far.
- **Startup recovery**: any session still `running` becomes `interrupted`. A call with no result gets `tool_result {ok: false, output: "interrupted by a server restart while running; it may have partly run"}`, or "cancelled before running" if a later call in the same message had not started either.

### Storage: SeaORM 2, SQLite or Postgres chosen at runtime

The backend is selected by `DATABASE_URL` (`sqlite://…` or `postgres://…`) in a single binary. Migrations use `sea-orm-migration`'s schema builder only, never raw SQL, so one migration set serves both backends, and run at startup. Event payloads are a `json` column.

```
sessions(id uuid pk, cwd text, model text, title text null,
         status text,            -- idle | running | interrupted
         created_at timestamptz, updated_at timestamptz)

events(session_id uuid fk, seq bigint, ts timestamptz,
       kind text, payload json,
       primary key (session_id, seq))
```

### Exposure and request filtering

Binds `127.0.0.1` only, plain HTTP, no authentication. A middleware on every route rejects with 403:

- a `Host` header other than `localhost:PORT`, `127.0.0.1:PORT` or `[::1]:PORT`, which defeats DNS rebinding;
- an `Origin` header, when present, other than the server's own origin, plus `http://localhost:5173` in debug builds for the Vite dev server.

Every mutating route takes a JSON body. A cross-origin page can only send one after a CORS preflight, and the server answers no preflight.

### Serving the client

- Dev: the Vite dev server proxies `/api` to the Rust server; the browser sees one origin.
- Release: with cargo feature `embed-web`, `web/dist` is embedded in the binary with `rust-embed` and served for every non-`/api` path, falling back to `index.html`. Without the feature, `cargo build` needs no JavaScript toolchain.

### Agent scope

- One OpenAI-compatible `/chat/completions` endpoint, streaming, with tool calls.
- Tools: `bash`, `read_file`, `write_file`, `edit_file`. No sandbox, no approval step. Tool calls in one model reply run in order, one at a time.
- Model reasoning is read from `reasoning_content` or `reasoning`, streamed as `reasoning_delta` and stored on `assistant_message`. It is sent back as `reasoning_content` only when the client's `send_reasoning` is set, off by default: `DeepSeek` answers HTTP 400 on a tool-carrying request that omits it ([Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode)), while a provider that rejects unknown message fields fails with it on.

### bash tool

- A fresh `bash -c <command>` per call, in its own process group, cwd = session cwd or the call's optional `workdir` (resolved against it). No state carries over between calls.
- Not a login shell; environment inherited from the server, plus `PAGER=cat`, `GIT_PAGER=cat`, `GIT_TERMINAL_PROMPT=0`. stdin is `/dev/null`.
- stdout and stderr share one pipe, so their interleaving is preserved. Lines are broadcast as `tool_progress` while the command runs.
- Timeout 120 s by default, overridable per call up to 600 s. Timeout and cancel kill the whole process group.
- Output returned to the model is capped at 30 KB, keeping head and tail around a marker giving the number of bytes dropped.

## Consequences

- **Anyone who can reach the port executes arbitrary commands as the user.** Localhost binding and Host/Origin filtering stop other machines and web pages, not other local users or processes. Any non-local exposure, or use on a shared machine, requires a new ADR adding authentication first.
- Every write to a session goes through its actor: a slow database write delays that session's stream, never another session.
- Supporting two backends means every store test runs twice: in-memory SQLite and Postgres via `TEST_DATABASE_URL`. A backend that is not tested is not supported.
- SQLite stores `json` as text: filtering on payload fields happens in Rust, not SQL.
- Token deltas are never stored. A crash mid-reply loses the partial assistant text; a cancel keeps it.
- After a crash during a tool call, the model is told the call may have partly run. Whether it did, nobody knows: the command's side effects are not tracked.
- The model cannot `cd` or `export` across bash calls; it chains commands or passes `workdir`.
- A server started outside a login shell (launchd, systemd) inherits a minimal `PATH`, and bash calls see the same.
- `send_reasoning` off is the wrong setting for a `DeepSeek`-backed model: its second call in a tool loop fails with HTTP 400 until the flag is turned on. Which setting an endpoint needs is found by trying one tool turn against it, not by reading its documentation alone.
- A release build needs the web client built first; CI builds `web/` before the `embed-web` binary.
- SSE is one-way. Features that need client input during a turn (approvals, steering) go through extra POST routes. Moving to WebSocket replaces one route and leaves the agent core untouched.
- A web client means no terminal UI in v0; debugging the loop alone goes through a headless CLI over the core crate.

## Alternatives considered

- **WebSocket for events.** Two-way, but nothing in v0 sends client messages mid-turn. Reconnection and replay must be hand-built, which `EventSource` gives for free. OpenHands uses this.
- **JSON-RPC (Codex app-server style).** Fits IDE extensions over stdio. A browser client gains nothing from it.
- **gRPC.** Browsers cannot call it without a grpc-web proxy.
- **Diesel.** Backend is fixed at compile time: both databases need `MultiConnection` or feature flags, plus per-backend migrations. Synchronous by default; async is a separate crate.
- **sqlx without an ORM.** Its `Any` driver loses compile-time query checking, and SQL dialect differences remain the caller's problem.
- **Persisting every token delta.** One row per token, for UI state no one replays.
- **Global auto-increment id as the cursor.** Needs one writer per session anyway: a Postgres sequence assigns ids before commit, so concurrent writers commit out of order and a client could skip an event. **`max(seq)+1` in a transaction**: collides under concurrent writers and needs retries.
- **Unbounded per-client queue for SSE.** A stalled tab grows server memory without limit. **Skipping lagged messages**: silently loses durable events.
- **Startup token (Jupyter-style).** Also blocks other local users, but `EventSource` cannot send headers, so the token travels in a cookie or the URL. Not needed on a single-user machine.
- **Filling missing tool results when rebuilding history, or dropping the unanswered assistant message.** The first leaves an invalid log that every reader must repair; the second makes the model forget a command it ran, and it may run it again.
- **One persistent shell per session.** Keeps `cd` and environment, but output and exit codes must be framed with markers, a command reading stdin or calling `exit` breaks the shell, and cancel ends up restarting it, losing the state anyway.
- **Discarding model reasoning.** Hides why the model acted.
- **Separate web and API origins with CORS.** Permanently widens the cross-origin surface the request filter exists to close. **Serving `web/dist` from disk**: simpler build, two artefacts to ship.

SSE event streams with REST commands are also the design used by OpenCode ([server docs](https://opencode.ai/docs/server)) and Goose ([`reply.rs`](https://github.com/block/goose/blob/main/crates/goose-server/src/routes/reply.rs)).
