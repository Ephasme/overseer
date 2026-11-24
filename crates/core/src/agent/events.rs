use crate::llm::{ToolCall, Usage};

/// Why a turn stopped. The turn always ends with exactly one `TurnEnd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndReason {
    /// The model answered without asking for a tool.
    Done,
    /// The caller cancelled; a running command was killed.
    Cancelled,
    /// `AgentConfig::max_steps` model calls were made and the model still asked for tools.
    MaxIterations,
    /// The model endpoint failed, or a request could not be built.
    Error,
}

/// What a turn emits, in order. The durable ones are ADR 0001's event log; the others exist only
/// for the live stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
        /// Token usage the endpoint reported for this reply, when it reported any.
        usage: Option<Usage>,
        /// True when the turn was cancelled mid-reply; `text` is then what had arrived.
        interrupted: bool,
    },
    ToolResult {
        call_id: String,
        ok: bool,
        output: String,
    },
    TurnEnd {
        reason: TurnEndReason,
    },
    Error {
        message: String,
    },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    ToolProgress {
        call_id: String,
        line: String,
    },
}

impl AgentEvent {
    /// Durable events are the ones phase 5 stores and replays; everything else is live only.
    #[must_use]
    pub fn is_durable(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. }
                | Self::AssistantMessage { .. }
                | Self::ToolResult { .. }
                | Self::TurnEnd { .. }
                | Self::Error { .. }
        )
    }
}

/// Where a turn's events go. Phase 5's session actor implements it over its broadcast channel.
pub trait EventSink: Send {
    fn emit(&mut self, event: AgentEvent);
}

impl EventSink for Vec<AgentEvent> {
    fn emit(&mut self, event: AgentEvent) {
        self.push(event);
    }
}
