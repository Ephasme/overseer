use serde_json::Value;

/// A tool the model may call, sent in the request's `tools` array.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

/// A complete tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments exactly as the model produced them: a JSON text, not yet parsed or validated.
    pub arguments: String,
}

/// One message of the conversation sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        /// Sent back as `reasoning_content` only when `ClientConfig::send_reasoning` is set.
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// What one model call is given.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    /// Any value outside the four above, kept verbatim.
    Other(String),
}

impl FinishReason {
    /// Parses the wire value; anything undocumented is kept as [`FinishReason::Other`].
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// The assistant message one model call produced, assembled from all its deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub content: String,
    pub reasoning: Option<String>,
    /// In the order of the provider's `index`.
    pub tool_calls: Vec<ToolCall>,
    /// `None` when the provider ended the stream without sending one.
    pub finish_reason: Option<FinishReason>,
    /// `None` when the provider sent no usage chunk.
    pub usage: Option<Usage>,
}

/// One item of a model call's stream. `Done` is always the last item of a successful stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// A fragment of tool call `index`. `id` and `name` are set on the fragment that carried them,
    /// normally the first; `arguments` is the next piece of the arguments text, possibly empty.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Done(Completion),
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("request to the model failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("model endpoint returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("model provider reported an error mid-stream: {message}")]
    Provider { message: String },
    #[error("unparsable stream chunk {data:?}: {source}")]
    Decode {
        data: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("stream ended before the model finished")]
    Truncated,
    #[error("tool call {index} has no {field}")]
    InvalidToolCall { index: usize, field: &'static str },
}

#[cfg(test)]
mod tests {
    use super::FinishReason;

    #[test]
    fn parses_the_four_documented_finish_reasons() {
        assert_eq!(FinishReason::parse("stop"), FinishReason::Stop);
        assert_eq!(FinishReason::parse("length"), FinishReason::Length);
        assert_eq!(FinishReason::parse("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(
            FinishReason::parse("content_filter"),
            FinishReason::ContentFilter
        );
    }

    #[test]
    fn keeps_an_unknown_finish_reason_verbatim() {
        assert_eq!(
            FinishReason::parse("error"),
            FinishReason::Other("error".to_owned())
        );
    }
}
