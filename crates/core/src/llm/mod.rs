//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

mod assemble;
mod client;
mod sse;
mod types;
mod wire;

pub use assemble::Assembler;
pub use client::{ChatStream, Client, ClientConfig};
pub use sse::SseParser;
pub use types::{
    ChatRequest, Completion, FinishReason, LlmError, LlmEvent, Message, ToolCall, ToolDefinition,
    Usage,
};
