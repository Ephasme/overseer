//! One turn of the agent: model calls and tool calls until the model stops, as events.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::llm::{ChatRequest, Client, Completion, LlmEvent, ToolCall};
use crate::tools::ToolRegistry;

mod conversation;
mod events;

pub use conversation::Conversation;
pub use events::{AgentEvent, EventSink, TurnEndReason};

/// How a turn is bounded and what the model is told once, at the start.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: String,
    /// Model calls one turn may make before it is stopped.
    pub max_steps: usize,
}

impl AgentConfig {
    #[must_use]
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            max_steps: 25,
        }
    }
}

/// The prompt the CLI uses: where the tools run, and how to use them.
#[must_use]
pub fn default_system_prompt(cwd: &Path) -> String {
    format!(
        "You are overseer, a coding agent working in {}. Use the tools to inspect and change files \
         and to run commands. Read a file before editing it. Keep answers short.",
        cwd.display()
    )
}

/// What one model call produced, from the loop's point of view.
enum Reply {
    Done(Completion),
    /// The caller cancelled mid-stream; `text` and `reasoning` are what had arrived.
    Cancelled {
        text: String,
        reasoning: String,
    },
}

/// One conversation with one model and one tool registry.
pub struct Agent {
    client: Client,
    tools: ToolRegistry,
    conversation: Conversation,
    config: AgentConfig,
}

impl Agent {
    #[must_use]
    pub fn new(client: Client, tools: ToolRegistry, config: AgentConfig) -> Self {
        let conversation = Conversation::new(config.system_prompt.clone());
        Self {
            client,
            tools,
            conversation,
            config,
        }
    }

    /// The conversation so far, durable events only.
    #[must_use]
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// Runs one turn: the user's text, then model calls and tool calls until the model answers
    /// without a tool, the step cap is reached, or `cancel` fires.
    pub async fn run_turn(
        &mut self,
        input: &str,
        sink: &mut dyn EventSink,
        cancel: &CancellationToken,
    ) -> TurnEndReason {
        self.record(
            sink,
            AgentEvent::UserMessage {
                text: input.to_owned(),
            },
        );
        for _ in 0..self.config.max_steps {
            if cancel.is_cancelled() {
                return self.end(sink, TurnEndReason::Cancelled);
            }
            let request = ChatRequest {
                messages: self.conversation.messages(),
                tools: self.tools.definitions(),
            };
            let reply = match self.complete(&request, sink, cancel).await {
                Ok(Reply::Done(completion)) => completion,
                Ok(Reply::Cancelled { text, reasoning }) => {
                    self.record(
                        sink,
                        AgentEvent::AssistantMessage {
                            text,
                            reasoning: (!reasoning.is_empty()).then_some(reasoning),
                            tool_calls: Vec::new(),
                            interrupted: true,
                        },
                    );
                    return self.end(sink, TurnEndReason::Cancelled);
                }
                Err(message) => {
                    self.record(sink, AgentEvent::Error { message });
                    return self.end(sink, TurnEndReason::Error);
                }
            };
            let calls = reply.tool_calls.clone();
            self.record(
                sink,
                AgentEvent::AssistantMessage {
                    text: reply.content,
                    reasoning: reply.reasoning,
                    tool_calls: calls.clone(),
                    interrupted: false,
                },
            );
            if calls.is_empty() {
                return self.end(sink, TurnEndReason::Done);
            }
            for (index, call) in calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    self.cancel_remaining(sink, &calls[index..]);
                    return self.end(sink, TurnEndReason::Cancelled);
                }
                let call_id = call.id.clone();
                let outcome = {
                    let mut progress = |line: &str| {
                        sink.emit(AgentEvent::ToolProgress {
                            call_id: call_id.clone(),
                            line: line.to_owned(),
                        });
                    };
                    self.tools
                        .call(&call.name, &call.arguments, &mut progress, cancel)
                        .await
                };
                self.record(
                    sink,
                    AgentEvent::ToolResult {
                        call_id: call.id.clone(),
                        ok: outcome.ok,
                        output: outcome.output,
                    },
                );
                if cancel.is_cancelled() {
                    self.cancel_remaining(sink, &calls[index + 1..]);
                    return self.end(sink, TurnEndReason::Cancelled);
                }
            }
        }
        self.end(sink, TurnEndReason::MaxIterations)
    }

    /// One model call, streaming its events into `sink`.
    async fn complete(
        &self,
        request: &ChatRequest,
        sink: &mut dyn EventSink,
        cancel: &CancellationToken,
    ) -> Result<Reply, String> {
        let mut stream = self
            .client
            .stream(request)
            .await
            .map_err(|error| error.to_string())?;
        let mut text = String::new();
        let mut reasoning = String::new();
        loop {
            let next = tokio::select! {
                next = stream.next() => next,
                () = cancel.cancelled() => return Ok(Reply::Cancelled { text, reasoning }),
            };
            match next {
                None => return Err("model stream ended without a completion".to_owned()),
                Some(Err(error)) => return Err(error.to_string()),
                Some(Ok(LlmEvent::Done(completion))) => return Ok(Reply::Done(completion)),
                Some(Ok(LlmEvent::TextDelta(delta))) => {
                    text.push_str(&delta);
                    sink.emit(AgentEvent::TextDelta(delta));
                }
                Some(Ok(LlmEvent::ReasoningDelta(delta))) => {
                    reasoning.push_str(&delta);
                    sink.emit(AgentEvent::ReasoningDelta(delta));
                }
                Some(Ok(LlmEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments,
                })) => sink.emit(AgentEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments,
                }),
            }
        }
    }

    fn record(&mut self, sink: &mut dyn EventSink, event: AgentEvent) {
        self.conversation.push(event.clone());
        sink.emit(event);
    }

    fn end(&mut self, sink: &mut dyn EventSink, reason: TurnEndReason) -> TurnEndReason {
        self.record(sink, AgentEvent::TurnEnd { reason });
        reason
    }

    /// Answers the calls a cancelled turn did not run, so every call keeps its result.
    fn cancel_remaining(&mut self, sink: &mut dyn EventSink, calls: &[ToolCall]) {
        for call in calls {
            self.record(
                sink,
                AgentEvent::ToolResult {
                    call_id: call.id.clone(),
                    ok: false,
                    output: "cancelled before running".to_owned(),
                },
            );
        }
    }
}
