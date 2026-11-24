use graven::{Event, EventFamily, EventRegistry};
use serde::{Deserialize, Serialize};

/// `session.created`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    /// Directory the session works in.
    pub cwd: String,
    /// Model the session was created with.
    pub model: String,
}

/// `session.user_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    /// What the user typed.
    pub text: String,
}

/// One tool call inside an assistant message, as the model asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallPayload {
    /// Provider id of the call.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as the JSON string the model produced.
    pub arguments: String,
}

/// Token usage the endpoint reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsagePayload {
    /// Prompt tokens billed.
    pub prompt_tokens: u64,
    /// Completion tokens billed.
    pub completion_tokens: u64,
}

/// `session.assistant_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Visible text, empty when the model only called tools.
    pub text: String,
    /// Reasoning text, when the provider streamed any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Tool calls the model asked for.
    pub tool_calls: Vec<ToolCallPayload>,
    /// Usage, when the provider reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsagePayload>,
    /// The turn was cancelled or failed after this message.
    #[serde(default)]
    pub interrupted: bool,
}

/// `session.tool_result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// `ToolCallPayload::id` this answers.
    pub call_id: String,
    /// Whether the tool succeeded.
    pub ok: bool,
    /// Tool output, already truncated by the tool.
    pub output: String,
}

/// Why a turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    /// The model answered without calling a tool.
    Done,
    /// The user cancelled the turn.
    Cancelled,
    /// The iteration cap was hit.
    MaxIterations,
    /// The provider or the loop failed.
    Error,
}

/// `session.turn_end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    /// Why the turn stopped.
    pub reason: TurnEndReason,
}

/// `session.error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionError {
    /// What failed, as shown to the user.
    pub message: String,
}

/// Every durable event of one session stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SessionEvent {
    /// The session was created.
    Created(SessionCreated),
    /// The user sent a message.
    UserMessage(UserMessage),
    /// The model answered.
    AssistantMessage(AssistantMessage),
    /// A tool ran.
    ToolResult(ToolResult),
    /// A turn stopped.
    TurnEnd(TurnEnd),
    /// Something failed outside a tool.
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

/// The registry every read and write goes through. Rebuilding it per call is cheap: the families
/// are `fn` pointers and a few `Box`ed decoders.
///
/// # Errors
///
/// `InvalidEventFamily` when a type is registered twice or a revision is uncovered; neither can
/// happen with the six families below, and the test suite pins it.
pub fn registry() -> Result<EventRegistry<SessionEvent>, graven::Error> {
    EventRegistry::builder()
        .family(EventFamily::new(
            "session.created",
            1,
            SessionEvent::Created,
        ))
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
        .family(EventFamily::new(
            "session.turn_end",
            1,
            SessionEvent::TurnEnd,
        ))
        .family(EventFamily::new("session.error", 1, SessionEvent::Error))
        .build()
}

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
        assert_eq!(
            encoded.payload,
            serde_json::json!({ "cwd": "/tmp", "model": "m" })
        );
    }
}
