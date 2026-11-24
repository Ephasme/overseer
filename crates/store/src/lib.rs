//! The durable event log: graven over SQLite, one stream per session.

mod events;
mod sessions;
mod store;

pub use events::{
    AssistantMessage, SessionCreated, SessionError, SessionEvent, ToolCallPayload, ToolResult,
    TurnEnd, TurnEndReason, UsagePayload, UserMessage, registry,
};
pub use sessions::SessionSummary;
pub use store::Store;

/// Aggregate type recorded on every row.
pub const AGGREGATE: &str = "session";
