use crate::llm::Message;

use super::events::AgentEvent;

/// The model's view of the conversation, rebuilt from the durable events.
#[derive(Debug, Clone)]
pub struct Conversation {
    system: String,
    messages: Vec<Message>,
}

impl Conversation {
    #[must_use]
    pub fn new(system: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            messages: Vec::new(),
        }
    }

    /// Rebuilds a conversation from a stored event log, the way phase 5 resumes a session.
    #[must_use]
    pub fn from_events(system: impl Into<String>, events: &[AgentEvent]) -> Self {
        let mut conversation = Self::new(system);
        for event in events {
            conversation.push(event.clone());
        }
        conversation
    }

    /// The messages of one request: the system prompt, then every durable message in order.
    #[must_use]
    pub fn messages(&self) -> Vec<Message> {
        let mut messages = Vec::with_capacity(self.messages.len() + 1);
        messages.push(Message::System {
            content: self.system.clone(),
        });
        messages.extend(self.messages.iter().cloned());
        messages
    }

    /// Folds one durable event in; ephemeral events are ignored.
    pub fn push(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::UserMessage { text } => self.messages.push(Message::User { content: text }),
            AgentEvent::AssistantMessage {
                text,
                reasoning,
                tool_calls,
                ..
            } => self.messages.push(Message::Assistant {
                content: text,
                reasoning,
                tool_calls,
            }),
            AgentEvent::ToolResult {
                call_id, output, ..
            } => self.messages.push(Message::Tool {
                tool_call_id: call_id,
                content: output,
            }),
            AgentEvent::TurnEnd { .. }
            | AgentEvent::Error { .. }
            | AgentEvent::TextDelta(_)
            | AgentEvent::ReasoningDelta(_)
            | AgentEvent::ToolCallDelta { .. }
            | AgentEvent::ToolProgress { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::llm::{Message, ToolCall};

    use super::Conversation;
    use crate::agent::{AgentEvent, TurnEndReason};

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }
    }

    #[test]
    fn durable_events_become_messages_in_order() {
        let mut conversation = Conversation::new("sys");
        conversation.push(AgentEvent::UserMessage { text: "hi".into() });
        conversation.push(AgentEvent::AssistantMessage {
            text: String::new(),
            reasoning: Some("think".into()),
            tool_calls: vec![tool_call()],
            usage: None,
            interrupted: false,
        });
        conversation.push(AgentEvent::ToolResult {
            call_id: "call_1".into(),
            ok: true,
            output: "a.rs".into(),
        });
        assert_eq!(
            conversation.messages(),
            [
                Message::System {
                    content: "sys".into()
                },
                Message::User {
                    content: "hi".into()
                },
                Message::Assistant {
                    content: String::new(),
                    reasoning: Some("think".into()),
                    tool_calls: vec![tool_call()],
                },
                Message::Tool {
                    tool_call_id: "call_1".into(),
                    content: "a.rs".into()
                },
            ]
        );
    }

    #[test]
    fn ephemeral_and_end_events_add_no_message() {
        let mut conversation = Conversation::new("sys");
        conversation.push(AgentEvent::TextDelta("x".into()));
        conversation.push(AgentEvent::ReasoningDelta("y".into()));
        conversation.push(AgentEvent::ToolProgress {
            call_id: "c".into(),
            line: "z".into(),
        });
        conversation.push(AgentEvent::TurnEnd {
            reason: TurnEndReason::Done,
        });
        conversation.push(AgentEvent::Error {
            message: "boom".into(),
        });
        assert_eq!(conversation.messages().len(), 1);
    }

    #[test]
    fn rebuilding_from_events_matches_a_live_conversation() {
        let events = vec![
            AgentEvent::UserMessage { text: "hi".into() },
            AgentEvent::AssistantMessage {
                text: "hello".into(),
                reasoning: None,
                tool_calls: vec![],
                usage: None,
                interrupted: false,
            },
            AgentEvent::TurnEnd {
                reason: TurnEndReason::Done,
            },
        ];
        let mut live = Conversation::new("sys");
        for event in &events {
            live.push(event.clone());
        }
        assert_eq!(
            Conversation::from_events("sys", &events).messages(),
            live.messages()
        );
    }
}
