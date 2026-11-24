//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

mod types;

pub use types::{
    ChatRequest, Completion, FinishReason, LlmError, LlmEvent, Message, ToolCall, ToolDefinition,
    Usage,
};
