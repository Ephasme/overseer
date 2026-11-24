# Phase 4: event store on graven — implementation plan

> **For the implementing agent:** Execute one task at a time. Tick each checkbox
> (`- [ ]` → `- [x]`) as you finish a step, and stop to ask if any step is unclear —
> never guess.

**Goal:** `crates/store`, the durable event log ADR 0004 specifies: graven-rs over SQLite, one stream per session, the six durable event types at revision 1, and a folded session summary — with the same test suite passing against SQLite and the in-memory store.

**Architecture:** `crates/store` is a thin facade. `events.rs` defines the payload structs, the `SessionEvent` enum and its `EventRegistry`; `store.rs` wraps graven's `SqliteStore` and `InMemoryStore` in one enum and exposes `append`, `events`, `global` and `session_ids`; `sessions.rs` folds a stream into `SessionSummary`. Nothing above this crate knows graven exists.

**Tech Stack:** Rust 1.100 (edition 2024), graven pinned by rev, `sqlite` feature, tokio, serde, serde_json, jiff (graven's timestamp type), uuid.

**Spec:** [ADR 0004](../adr/0004-event-log-graven.md) (supersedes the storage section of [ADR 0001](../adr/0001-v0-architecture.md)), [rough plan](2026-11-24-overseer-v0.md) phase 4.

## Global Constraints

- Rust toolchain `1.100`, edition 2024, `unsafe_code = "forbid"`, clippy `pedantic` with `-D warnings`, no `#[allow]`.
- Dependency, exactly as ADR 0004 pins it:

  ```toml
  graven = { git = "ssh://git@github.com/Les-Sherpas/graven-rs.git", rev = "5b9c97c369e087c055357aad107709493f7fbcb4", features = ["sqlite"] }
  ```

- No dependency without code that uses it; `cargo shear` must pass.
- SQLite only. No SeaORM, no `TEST_DATABASE_URL`, no Postgres.
- Event type strings are immutable: `session.created`, `session.user_message`, `session.assistant_message`, `session.tool_result`, `session.turn_end`, `session.error`, all at revision 1.
- Aggregate type is the string `session`; stream id is a UUID v4 string.
- Writes go through `EventStore::append` with an `ExpectedVersion`, never through `decide`. The caller owns the expected version.
- Ephemeral events are not stored.
- The gate is `just check-rust`; CI runs it unchanged.
- Commit messages in English, Conventional Commits.

## File map

| Path | Responsibility | Task |
|---|---|---|
| `crates/store/Cargo.toml` | the graven dependency and its siblings | 1 |
| `crates/store/src/events.rs` | payload structs, `SessionEvent`, `registry()` | 1 |
| `crates/store/src/lib.rs` | re-exports; `AGGREGATE` constant | 1 |
| `crates/store/src/store.rs` | `Store` over SQLite and memory: append, read, ids | 2 |
| `crates/store/src/sessions.rs` | `SessionSummary` and its fold | 3 |
| `crates/store/tests/graven_store.rs` | one suite, two backends | 2, 3 |
| `.cargo/config.toml` | `net.git-fetch-with-cli = true` for the private repo | 4 |
| `.github/workflows/{ci,release}.yml` | SSH agent for the private dependency | 4 |
| `docs/adr/0001-v0-architecture.md` | storage section points at ADR 0004 | 4 |
| `docs/plans/2026-11-24-overseer-v0.md` | phase 4 row | 4 |

---

### Task 1: Event catalog and registry

**Files:**
- Create: `crates/store/Cargo.toml`, `crates/store/src/lib.rs`, `crates/store/src/events.rs`

**Interfaces:**
- Consumes: nothing from this repo; graven's `Event`, `EventFamily`, `EventRegistry`, `TimedEvent`, `Error`.
- Produces: `overseer_store::{SessionEvent, SessionCreated, UserMessage, AssistantMessage, ToolCallPayload, UsagePayload, ToolResult, TurnEnd, TurnEndReason, SessionError, registry(), AGGREGATE}`, with `registry() -> Result<EventRegistry<SessionEvent>, graven::Error>`.

- [ ] **Step 1: Write the failing test**

Create `crates/store/src/events.rs` with the test module below and nothing else; `cargo nextest run -p overseer-store` must fail to compile (`SessionEvent` unresolved).

```rust
#[cfg(test)]
mod tests {
    use graven::Event;

    use super::{AssistantMessage, SessionCreated, SessionEvent, ToolCallPayload, registry};

    #[test]
    fn every_event_type_is_registered_at_revision_one() {
        let registry = registry().expect("registry builds");
        for event in [
            SessionEvent::Created(SessionCreated {
                cwd: "/tmp".into(),
                model: "m".into(),
            }),
            SessionEvent::AssistantMessage(AssistantMessage {
                text: "hi".into(),
                reasoning: None,
                tool_calls: vec![ToolCallPayload {
                    id: "c".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                usage: None,
                interrupted: false,
            }),
        ] {
            let encoded = registry.encode(&event).expect("encodes");
            assert_eq!(encoded.event_revision, 1);
            assert_eq!(encoded.event_type, event.event_type());
        }
    }

    #[test]
    fn the_payload_is_the_bare_variant() {
        let registry = registry().expect("registry builds");
        let encoded = registry
            .encode(&SessionEvent::Created(SessionCreated {
                cwd: "/tmp".into(),
                model: "m".into(),
            }))
            .expect("encodes");
        assert_eq!(encoded.payload, serde_json::json!({ "cwd": "/tmp", "model": "m" }));
    }
}
```

- [ ] **Step 2: Write the crate manifest**

`crates/store/Cargo.toml`:

```toml
[package]
name = "overseer-store"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[lints]
workspace = true

[dependencies]
graven.workspace = true
jiff.workspace = true
serde.workspace = true
serde_json.workspace = true
uuid.workspace = true
```

Workspace additions in the root `Cargo.toml`, inside `[workspace.dependencies]`:

```toml
graven = { git = "ssh://git@github.com/Les-Sherpas/graven-rs.git", rev = "5b9c97c369e087c055357aad107709493f7fbcb4", features = ["sqlite"] }
jiff = "1.0.1"
uuid = { version = "1.29.3", features = ["v4"] }
```

- [ ] **Step 3: Write the catalog**

`crates/store/src/lib.rs`:

```rust
//! The durable event log: graven over SQLite, one stream per session.

mod events;

pub use events::{
    AssistantMessage, SessionCreated, SessionError, SessionEvent, ToolCallPayload, ToolResult,
    TurnEnd, TurnEndReason, UsagePayload, UserMessage, registry,
};
/// Aggregate type recorded on every row.
pub const AGGREGATE: &str = "session";
```

`crates/store/src/events.rs` (above the test module):

```rust
use graven::{Event, EventFamily, EventRegistry};
use serde::{Deserialize, Serialize};

/// `session.created`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub cwd: String,
    pub model: String,
}

/// `session.user_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub text: String,
}

/// One tool call inside an assistant message, as the model asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Token usage the endpoint reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsagePayload {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// `session.assistant_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCallPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsagePayload>,
    #[serde(default)]
    pub interrupted: bool,
}

/// `session.tool_result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub ok: bool,
    pub output: String,
}

/// Why a turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Done,
    Cancelled,
    MaxIterations,
    Error,
}

/// `session.turn_end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    pub reason: TurnEndReason,
}

/// `session.error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionError {
    pub message: String,
}

/// Every durable event of one session stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SessionEvent {
    Created(SessionCreated),
    UserMessage(UserMessage),
    AssistantMessage(AssistantMessage),
    ToolResult(ToolResult),
    TurnEnd(TurnEnd),
    Error(SessionError),
}

impl Event for SessionEvent {
    fn event_type(&self) -> &'static str {
        match self {
            Self::Created(_) => "session.created",
            Self::UserMessage(_) => "session.user_message",
            Self::AssistantMessage(_) => "session.assistant_message",
            Self::ToolResult(_) => "session.tool_result",
            Self::TurnEnd(_) => "session.turn_end",
            Self::Error(_) => "session.error",
        }
    }
}

/// The registry every read and write goes through. Rebuilding it per call is cheap; the families
/// are six `fn` pointers.
pub fn registry() -> Result<EventRegistry<SessionEvent>, graven::Error> {
    EventRegistry::builder()
        .family(EventFamily::new("session.created", 1, SessionEvent::Created))
        .family(EventFamily::new(
            "session.user_message",
            1,
            SessionEvent::UserMessage,
        ))
        .family(EventFamily::new(
            "session.assistant_message",
            1,
            SessionEvent::AssistantMessage,
        ))
        .family(EventFamily::new(
            "session.tool_result",
            1,
            SessionEvent::ToolResult,
        ))
        .family(EventFamily::new("session.turn_end", 1, SessionEvent::TurnEnd))
        .family(EventFamily::new("session.error", 1, SessionEvent::Error))
        .build()
}
```

`src/store.rs` and `src/sessions.rs` do not exist yet: Task 2 and Task 3 create them and add their `mod` line and re-export to `lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run -p overseer-store`
Expected: PASS, `2 tests run: 2 passed`.

- [ ] **Step 5: Run the gate**

Run: `just check-rust`
Expected: exit 0. `cargo deny` must allow graven's dependencies: `rusqlite` (MIT), `jiff` (Unlicense OR MIT), `uuid` (Apache-2.0 OR MIT); all are already on the allow list.

---

### Task 2: One facade over SQLite and memory

**Files:**
- Create: `crates/store/src/store.rs`; modify `crates/store/src/lib.rs` (add `mod store;` + re-export), `crates/store/Cargo.toml` (dev-dependencies), `crates/store/tests/graven_store.rs`

**Interfaces:**
- Consumes: `crate::{AGGREGATE, SessionEvent, registry}`; graven's `AppendRequest`, `AppendResult`, `Error`, `EventMetadata`, `EventStore`, `EventToAppend`, `ExpectedVersion`, `GlobalReadOptions`, `InMemoryStore`, `SqliteStore`, `StoredEvent`, `TimedEvent`.
- Produces:

  ```rust
  pub enum Store { Sqlite(Box<SqliteStore>), Memory(InMemoryStore) }
  impl Store {
      pub async fn open(path: impl Into<PathBuf>) -> Result<Self, Error>;
      pub fn memory() -> Self;
      pub async fn append(&self, stream_id: &str, expected: ExpectedVersion, events: &[SessionEvent]) -> Result<AppendResult, Error>;
      pub async fn events(&self, stream_id: &str) -> Result<Vec<TimedEvent<SessionEvent>>, Error>;
      pub async fn global(&self, from: u64, limit: usize) -> Result<Vec<StoredEvent>, Error>;
      pub async fn session_ids(&self) -> Result<Vec<String>, Error>;
      pub async fn version(&self, stream_id: &str) -> Result<u64, Error>;
      pub fn decode(stored: &[StoredEvent]) -> Result<Vec<TimedEvent<SessionEvent>>, Error>;
  }
  ```

- [ ] **Step 1: Write the failing test**

`crates/store/tests/graven_store.rs`:

```rust
use graven::ExpectedVersion;
use overseer_store::{SessionCreated, SessionEvent, Store, UserMessage};

fn created(cwd: &str) -> SessionEvent {
    SessionEvent::Created(SessionCreated {
        cwd: cwd.to_owned(),
        model: "m".to_owned(),
    })
}

fn user(text: &str) -> SessionEvent {
    SessionEvent::UserMessage(UserMessage {
        text: text.to_owned(),
    })
}

async fn store_suite(store: Store) {
    let id = "0199f0a0-0000-7000-8000-000000000001";

    assert!(store.events(id).await.expect("reads").is_empty());
    assert_eq!(store.version(id).await.expect("reads"), 0);

    let appended = store
        .append(id, ExpectedVersion::NoStream, &[created("/tmp"), user("hi")])
        .await
        .expect("appends");
    assert_eq!(appended.new_version, 2);

    let events = store.events(id).await.expect("reads");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, created("/tmp"));
    assert_eq!(events[1].event, user("hi"));

    let appended = store
        .append(
            id,
            ExpectedVersion::Exact(2),
            &[SessionEvent::TurnEnd(overseer_store::TurnEnd {
                reason: overseer_store::TurnEndReason::Done,
            })],
        )
        .await
        .expect("appends");
    assert_eq!(appended.new_version, 3);

    let rejected = store
        .append(id, ExpectedVersion::Exact(2), &[user("stale")])
        .await;
    assert!(rejected.is_err(), "a stale expected version must be refused");
    assert_eq!(store.version(id).await.expect("reads"), 3);

    assert_eq!(store.session_ids().await.expect("lists"), vec![id.to_owned()]);
}

#[tokio::test]
async fn sqlite_backend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("events.db")).await.expect("opens");
    store_suite(store).await;
}

#[tokio::test]
async fn memory_backend() {
    store_suite(Store::memory()).await;
}

#[tokio::test]
async fn the_global_log_is_ordered_and_resumable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("events.db")).await.expect("opens");
    store
        .append("aaaaaaaa-0000-7000-8000-000000000001", ExpectedVersion::NoStream, &[created("/a")])
        .await
        .expect("appends");
    store
        .append("aaaaaaaa-0000-7000-8000-000000000002", ExpectedVersion::NoStream, &[created("/b")])
        .await
        .expect("appends");

    let all = store.global(0, 100).await.expect("reads");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].stream_id, "aaaaaaaa-0000-7000-8000-000000000001");
    assert_eq!(all[1].stream_id, "aaaaaaaa-0000-7000-8000-000000000002");
    assert_eq!(all[1].global_position, all[0].global_position + 1);

    let tail = store
        .global(all[0].global_position, 100)
        .await
        .expect("reads");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].stream_id, "aaaaaaaa-0000-7000-8000-000000000002");
}
```

`crates/store/Cargo.toml` gains:

```toml
[dev-dependencies]
tempfile.workspace = true
tokio = { workspace = true, features = ["macros", "rt"] }
```

Root `Cargo.toml` `[workspace.dependencies]` gains `tempfile = "3.27.0"` unless the workspace already pins it; reuse the existing entry when present (`cargo metadata` shows it).

- [ ] **Step 2: Run it, watch it fail**

Run: `cargo nextest run -p overseer-store`
Expected: compile error, `Store` not exported.

- [ ] **Step 3: Write `crates/store/src/store.rs`**

```rust
use std::path::PathBuf;

use graven::{
    AppendRequest, AppendResult, Error, EventMetadata, EventStore, EventToAppend, ExpectedVersion,
    GlobalReadOptions, InMemoryStore, SqliteStore, StoredEvent, TimedEvent,
};
use uuid::Uuid;

use crate::{AGGREGATE, SessionEvent, registry};

/// Where the log lives: one SQLite file, or memory for tests.
pub enum Store {
    Sqlite(Box<SqliteStore>),
    Memory(InMemoryStore),
}

impl Store {
    /// Opens (creating it) the SQLite file, then creates the missing tables.
    ///
    /// # Errors
    ///
    /// Whatever `SqliteStore::open` or `migrate` returns: unreadable path, bad schema.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path: PathBuf = path.into();
        let store = SqliteStore::builder(path).open()?;
        store.migrate().await?;
        Ok(Self::Sqlite(Box::new(store)))
    }

    /// An empty log that disappears with the process.
    #[must_use]
    pub fn memory() -> Self {
        Self::Memory(InMemoryStore::new())
    }

    /// Appends `events` to `stream_id`, after checking `expected`.
    ///
    /// # Errors
    ///
    /// `UnknownEventType`/`InvalidEventPayload` when an event cannot be stamped; the store's own
    /// error on a version conflict or a write failure.
    pub async fn append(
        &self,
        stream_id: &str,
        expected: ExpectedVersion,
        events: &[SessionEvent],
    ) -> Result<AppendResult, Error> {
        let registry = registry()?;
        let mut rows = Vec::with_capacity(events.len());
        for event in events {
            let encoded = registry.encode(event)?;
            rows.push(EventToAppend {
                event_id: Uuid::new_v4().to_string(),
                event_type: encoded.event_type.to_owned(),
                event_revision: encoded.event_revision,
                payload: encoded.payload,
                occurred_at: jiff::Timestamp::now(),
            });
        }
        let request = AppendRequest {
            stream_id: stream_id.to_owned(),
            aggregate_type: AGGREGATE.to_owned(),
            expected,
            events: rows,
            metadata: EventMetadata {
                correlation_id: Uuid::new_v4().to_string(),
                causation_id: None,
                actor: Some("overseer".to_owned()),
            },
        };
        match self {
            Self::Sqlite(store) => store.append(request).await,
            Self::Memory(store) => store.append(request).await,
        }
    }

    /// The stream's events, decoded to the latest shape, in version order.
    ///
    /// # Errors
    ///
    /// The store's read error; `UnknownEventType`/`InvalidEventPayload` when a row cannot be
    /// decoded.
    pub async fn events(&self, stream_id: &str) -> Result<Vec<TimedEvent<SessionEvent>>, Error> {
        let stored = match self {
            Self::Sqlite(store) => store.read_stream(stream_id).await?,
            Self::Memory(store) => store.read_stream(stream_id).await?,
        };
        Self::decode(&stored)
    }

    /// Session events after `from`, in commit order, at most `limit`.
    ///
    /// # Errors
    ///
    /// The store's read error.
    pub async fn global(&self, from: u64, limit: usize) -> Result<Vec<StoredEvent>, Error> {
        let options = GlobalReadOptions {
            from_position: from,
            limit,
            aggregate_type: Some(AGGREGATE.to_owned()),
        };
        match self {
            Self::Sqlite(store) => store.read_global(&options).await,
            Self::Memory(store) => store.read_global(&options).await,
        }
    }

    /// Every session id, in order of first event.
    ///
    /// # Errors
    ///
    /// The store's read error.
    pub async fn session_ids(&self) -> Result<Vec<String>, Error> {
        match self {
            Self::Sqlite(store) => store.read_stream_ids(AGGREGATE).await,
            Self::Memory(store) => store.read_stream_ids(AGGREGATE).await,
        }
    }

    /// The stream's current version; 0 when it does not exist.
    ///
    /// # Errors
    ///
    /// The store's read error.
    pub async fn version(&self, stream_id: &str) -> Result<u64, Error> {
        let stored = match self {
            Self::Sqlite(store) => store.read_stream(stream_id).await?,
            Self::Memory(store) => store.read_stream(stream_id).await?,
        };
        Ok(stored.last().map_or(0, |row| row.stream_version))
    }

    /// Decodes stored rows with the registry.
    ///
    /// # Errors
    ///
    /// `UnknownEventType`, `UnknownEventRevision` or `InvalidEventPayload`.
    pub fn decode(stored: &[StoredEvent]) -> Result<Vec<TimedEvent<SessionEvent>>, Error> {
        let registry = registry()?;
        stored.iter().map(|row| registry.decode(row)).collect()
    }
}
```

`lib.rs` gains `mod store;` and `pub use store::Store;`.

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run -p overseer-store`
Expected: PASS, `4 tests run: 4 passed`.

- [ ] **Step 5: The gate**

Run: `just check-rust`
Expected: exit 0.

---

### Task 3: Session summary folded from the stream

**Files:**
- Create: `crates/store/src/sessions.rs`; modify `crates/store/src/lib.rs`, `crates/store/src/store.rs` (a `summary` method), `crates/store/tests/graven_store.rs`

**Interfaces:**
- Consumes: `SessionEvent`, `TurnEndReason`.
- Produces:

  ```rust
  pub struct SessionSummary { pub cwd: String, pub model: String, pub turns: u32, pub last_turn: Option<TurnEndReason>, pub open_turn: bool }
  impl SessionSummary { pub fn fold(events: &[SessionEvent]) -> Option<Self> }
  impl Store { pub async fn summary(&self, stream_id: &str) -> Result<Option<SessionSummary>, Error>; }
  ```

- [ ] **Step 1: Write the failing test**

Add to `crates/store/tests/graven_store.rs`:

```rust
#[tokio::test]
async fn a_session_summary_folds_the_stream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("events.db")).await.expect("opens");
    let id = "bbbbbbbb-0000-7000-8000-000000000001";

    assert!(store.summary(id).await.expect("reads").is_none());

    store
        .append(id, ExpectedVersion::NoStream, &[created("/repo")])
        .await
        .expect("appends");
    let summary = store.summary(id).await.expect("reads").expect("exists");
    assert_eq!(summary.cwd, "/repo");
    assert_eq!(summary.model, "m");
    assert_eq!(summary.turns, 0);
    assert!(!summary.open_turn);

    store
        .append(id, ExpectedVersion::Exact(1), &[user("hi")])
        .await
        .expect("appends");
    let summary = store.summary(id).await.expect("reads").expect("exists");
    assert_eq!(summary.turns, 1);
    assert!(summary.open_turn);
    assert_eq!(summary.last_turn, None);

    store
        .append(
            id,
            ExpectedVersion::Exact(2),
            &[SessionEvent::TurnEnd(overseer_store::TurnEnd {
                reason: overseer_store::TurnEndReason::Cancelled,
            })],
        )
        .await
        .expect("appends");
    let summary = store.summary(id).await.expect("reads").expect("exists");
    assert!(!summary.open_turn);
    assert_eq!(
        summary.last_turn,
        Some(overseer_store::TurnEndReason::Cancelled)
    );
}
```

- [ ] **Step 2: Run it, watch it fail**

Run: `cargo nextest run -p overseer-store a_session_summary`
Expected: compile error, no `summary` method.

- [ ] **Step 3: Write `crates/store/src/sessions.rs`**

```rust
use crate::{SessionEvent, TurnEndReason};

/// What a session stream says about itself, without replaying every message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Directory the session works in, from `session.created`.
    pub cwd: String,
    /// Model the session was created with, from `session.created`.
    pub model: String,
    /// Turns started so far.
    pub turns: u32,
    /// Reason the last turn ended; `None` before the first one ends.
    pub last_turn: Option<TurnEndReason>,
    /// The last turn has started and has no `session.turn_end` yet: running, or interrupted if the
    /// log is being read after a restart.
    pub open_turn: bool,
}

impl SessionSummary {
    /// Folds a stream, in order; `None` until the `session.created` row.
    #[must_use]
    pub fn fold(events: &[SessionEvent]) -> Option<Self> {
        let mut summary: Option<Self> = None;
        for event in events {
            match event {
                SessionEvent::Created(created) => {
                    summary = Some(Self {
                        cwd: created.cwd.clone(),
                        model: created.model.clone(),
                        turns: 0,
                        last_turn: None,
                        open_turn: false,
                    });
                }
                SessionEvent::UserMessage(_) => {
                    if let Some(summary) = summary.as_mut()
                        && !summary.open_turn
                    {
                        summary.turns += 1;
                        summary.open_turn = true;
                    }
                }
                SessionEvent::TurnEnd(end) => {
                    if let Some(summary) = summary.as_mut() {
                        summary.open_turn = false;
                        summary.last_turn = Some(end.reason);
                    }
                }
                SessionEvent::AssistantMessage(_)
                | SessionEvent::ToolResult(_)
                | SessionEvent::Error(_) => {}
            }
        }
        summary
    }
}
```

`lib.rs` gains `mod sessions;` and `pub use sessions::SessionSummary;`.

`store.rs` gains:

```rust
    /// The folded summary of a stream; `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// The store's read error, or a decode error.
    pub async fn summary(&self, stream_id: &str) -> Result<Option<SessionSummary>, Error> {
        let events = self.events(stream_id).await?;
        Ok(SessionSummary::fold(
            &events.into_iter().map(|timed| timed.event).collect::<Vec<_>>(),
        ))
    }
```

with `use crate::SessionSummary;` at the top.

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run -p overseer-store`
Expected: PASS, `5 tests run: 5 passed`.

- [ ] **Step 5: The gate**

Run: `just check-rust`
Expected: exit 0.

---

### Task 4: Fetch the private dependency in CI, and point the docs at the new store

**Files:**
- Create: `.cargo/config.toml`
- Modify: `Cargo.toml` (path dependency for the workspace member — Task 1 already added the graven entry), `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `docs/adr/0001-v0-architecture.md`, `docs/plans/2026-11-24-overseer-v0.md`

**Interfaces:** none.

- [ ] **Step 1: Prove the fetch works locally**

Run: `cargo fetch`
Expected: graven resolves at the pinned rev from the local cargo git database (`~/.cargo/git/db/graven-rs-*`, keyed by the SSH URL). A machine without it clones over SSH, which needs a GitHub key with read access to `Les-Sherpas/graven-rs`. With no network at all, `cargo fetch --offline` still works once that database exists.

- [ ] **Step 2: `.cargo/config.toml`**

```toml
[net]
# The graven dependency is a private repository: cargo's built-in libgit2 fetch cannot use the
# git credential helper, the `git` binary can.
git-fetch-with-cli = true
```

- [ ] **Step 3: Make CI fail closed when the key is missing**

The dependency is fetched over SSH, so CI needs an agent. Add this step right after `actions/checkout` in the `rust` and `build` jobs of `.github/workflows/ci.yml` and in the `assets` job of `.github/workflows/release.yml`:

```yaml
      - name: Authenticate the private graven dependency
        env:
          GRAVEN_SSH_KEY: ${{ secrets.GRAVEN_SSH_KEY }}
        run: |
          if [ -z "$GRAVEN_SSH_KEY" ]; then
            echo "GRAVEN_SSH_KEY is not set: the build cannot fetch the private graven-rs repository" >&2
            exit 1
          fi
          eval "$(ssh-agent -s)"
          printf '%s\n' "$GRAVEN_SSH_KEY" | ssh-add -
          mkdir -p ~/.ssh
          ssh-keyscan github.com >> ~/.ssh/known_hosts
```

A secret named `GRAVEN_SSH_KEY` must hold an ed25519 key whose public half is a read-only deploy key on `Les-Sherpas/graven-rs`:

```bash
ssh-keygen -t ed25519 -N '' -C overseer-ci -f /tmp/graven-ci
GH_TOKEN=$(gh auth token -u loup-peluso) gh repo deploy-key add /tmp/graven-ci.pub \
  -R Les-Sherpas/graven-rs --title overseer-ci --allow-read-only
GH_TOKEN=$(gh auth token -u Ephasme) gh secret set GRAVEN_SSH_KEY \
  -R Ephasme/overseer < /tmp/graven-ci
rm /tmp/graven-ci /tmp/graven-ci.pub
```

Both commands need Loup's go-ahead: the first grants a machine read access to a work repository, the second installs the matching private key as a repository secret. The workflow fails loudly until they run.

- [ ] **Step 4: Keep cargo-deny honest about the private crate**

`graven`'s manifest declares no `license` field and the crate is not published, so `cargo deny` reports it as unlicensed. Take it out of the graph rather than allowing unlicensed crates repository-wide, in `deny.toml`:

```toml
[graph]
all-features = true
# graven's manifest carries no license field and it is not published: it is our own private
# repository, so it is out of the license and ban graph rather than allowed crate-wide.
exclude = ["graven"]
```

Do not add `allow-git`: an excluded crate never reaches the source check, and cargo-deny warns when an allowance matches nothing.

- [ ] **Step 5: ADR 0001 storage pointer**

In `docs/adr/0001-v0-architecture.md`, in the "Storage" bullet of the decisions section, append one sentence:

> Superseded by [ADR 0004](0004-event-log-graven.md): the log is graven over SQLite, not SeaORM over Postgres.

- [ ] **Step 6: Rough plan phase 4 row**

In `docs/plans/2026-11-24-overseer-v0.md`, the phase 4 row's Notes cell gains:

> store shipped as its own crate on graven; see [phase 4 store plan](2026-11-24-phase-4-store-graven.md).

- [ ] **Step 7: The gate**

Run: `just check`
Expected: exit 0.
