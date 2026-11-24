# 4. Event log: graven over SQLite

- Status: Accepted
- Date: 2026-11-24
- Supersedes: the storage section of [ADR 0001](0001-v0-architecture.md) — the SeaORM dependency, the `sessions`/`events` DDL, the two-backend rule and the migration style.

## Context

[ADR 0001](0001-v0-architecture.md) chose SeaORM with a runtime switch between SQLite and Postgres. Phase 4 exposes what that decision deferred: stored events are a schema that changes as the protocol grows (`assistant_message` already carries `usage` and `interrupted`), replay must decode rows written by older code, and stream position, global order, a test double and projections are all event-store concerns rather than table concerns.

[graven-rs](https://github.com/Les-Sherpas/graven-rs) is a Rust event-sourcing library with event types, per-type revisions and upcasters, per-stream versions with an expected-version check, a gap-free global position, inline projections, catch-up projectors, an in-memory store and a bundled-SQLite store. overlord already depends on it at rev `5b9c97c`.

## Decision

### graven is the log, SQLite is the backend

`crates/store` depends on `graven` from `ssh://git@github.com/Les-Sherpas/graven-rs.git`, pinned to rev `5b9c97c369e087c055357aad107709493f7fbcb4`, feature `sqlite`, and wraps `SqliteStore`. SeaORM and its migrations are dropped: graven creates its five tables through `SqliteStore::migrate`, and overseer writes none. SQLite only — v0 is a single-user localhost harness, and graven-rs has no Postgres store.

### One stream per session

- Aggregate type `session`; stream id = session id, a UUID v4 generated at creation. `stream_version` is ADR 0001's `seq` and the SSE cursor; `global_position` orders the whole log.
- Durable events, all at revision 1:
  - `session.created {cwd, model}`
  - `session.user_message {text}`
  - `session.assistant_message {text, reasoning?, tool_calls, usage?, interrupted}`
  - `session.tool_result {call_id, ok, output}`
  - `session.turn_end {reason}`
  - `session.error {message}`
- Ephemeral events are never stored.
- An event type string is immutable. A payload change bumps the family's revision and registers an upcaster from the previous shape; stored rows are never rewritten, so replay of old sessions keeps working.

### Appends, not decisions

Writes go through `EventStore::append` with an `ExpectedVersion` (`NoStream` for `session.created`), not through `decide`. The turn loop emits events as they happen and has no command that folds a stream into one batch; the caller owns the expected version, exactly as ADR 0001's actor owns its `seq`. The unique `(stream_id, stream_version)` constraint is the safety net.

### Session metadata is folded, not stored

There is no `sessions` table.

- `cwd` and `model` come from `session.created`.
- `created_at` is its `occurred_at`; `updated_at` is the last durable event's `occurred_at`.
- `title` stays absent in v0.
- Status is derived from the last event of the stream: `running` while a turn is open (anything after a `user_message` that is not a `turn_end`), otherwise `idle`. A start sees an open turn as `interrupted` and appends the repair events ADR 0001 specifies.

### Two backends in the crate, both real

`crates/store` exposes one `Store` over a private enum of `SqliteStore` and `InMemoryStore`, and the same test suite runs against both — a temp database file and memory. Adding Postgres later means a new variant here and an `EventStore` implementation in graven-rs; nothing else in overseer changes.

## Consequences

- ADR 0001's storage DDL, its schema-builder-only migration rule and its "every store test runs on SQLite and Postgres" rule are replaced. `TEST_DATABASE_URL` and the Postgres service container in CI disappear.
- SQLite holds one writer for the whole database (`BEGIN IMMEDIATE`): a slow append delays every session, not only the one being written. ADR 0001's isolation holds for actor queues, not for the disk.
- The dependency is private and pinned: a build needs access to `Les-Sherpas/graven-rs`, and upgrades are explicit rev bumps.
- rusqlite is synchronous, so an append blocks the calling task for its duration. Acceptable at one actor per session and small writes; a bulk operation later goes through `spawn_blocking`.
- Timestamps are `jiff::Timestamp`, stored as TEXT; there is no `timestamptz` and no JSON column type.
- `session.created` is durable but not a client-facing protocol event; keeping it out of the SSE union is the server's job.
- A decode of an unknown event type or an uncovered revision fails the replay rather than skipping the row, so a bad deployment is loud instead of lossy.

## Alternatives considered

- **SeaORM as ADR 0001 decided.** Reimplements stream versions, upcasting, global order and an in-memory test store, and doubles every store test across two backends.
- **Postgres-only.** Convenient for a hosted mode that does not exist, inconvenient locally: a service to install, run and upgrade for a single-user app. graven-rs has no Postgres store to reuse anyway.
- **Hand-rolled rusqlite log with an `event_revision` column.** Small at first, then grows into graven's registry, versions and conflict checks one bug at a time.
- **A `sessions` read model maintained by an inline projection.** Consistent only through `decide`, which overseer does not use; folding six event types costs one read and removes a second source of truth.
- **graven (TypeScript) behind a sidecar.** A second runtime and a network hop on every append.
