use std::path::PathBuf;

use graven::{
    AppendRequest, AppendResult, Error, EventMetadata, EventStore, EventToAppend, ExpectedVersion,
    GlobalReadOptions, InMemoryStore, SqliteStore, StoredEvent, TimedEvent,
};
use uuid::Uuid;

use crate::{AGGREGATE, SessionEvent, SessionSummary, registry};

/// Where the log lives: one SQLite file, or memory for tests.
pub enum Store {
    /// Durable, one file.
    Sqlite(Box<SqliteStore>),
    /// Ephemeral, for tests.
    Memory(InMemoryStore),
}

impl Store {
    /// Opens (creating it) the SQLite file, then creates the missing tables.
    ///
    /// # Errors
    ///
    /// Whatever opening or migrating the file returns: unreadable path, bad schema.
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
    /// `UnknownEventType` or `InvalidEventPayload` when an event cannot be stamped; the store's own
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
    /// The store's read error; `UnknownEventType`, `UnknownEventRevision` or
    /// `InvalidEventPayload` when a row cannot be decoded.
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

    /// The folded summary of a stream; `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// The store's read error, or a decode error.
    pub async fn summary(&self, stream_id: &str) -> Result<Option<SessionSummary>, Error> {
        let events = self.events(stream_id).await?;
        Ok(SessionSummary::fold(
            &events
                .into_iter()
                .map(|timed| timed.event)
                .collect::<Vec<_>>(),
        ))
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
