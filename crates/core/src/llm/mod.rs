//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

mod sse;
mod types;

pub use sse::SseParser;
pub use types::{
    ChatRequest, Completion, FinishReason, LlmError, LlmEvent, Message, ToolCall, ToolDefinition,
    Usage,
};
