use std::collections::BTreeMap;

use super::types::{Completion, FinishReason, LlmError, LlmEvent, ToolCall, Usage};
use super::wire::Chunk;

/// Turns the `data` payloads of a chat completion stream into `LlmEvent`s and the final
/// `Completion`. Pure: no I/O, so recorded streams can be replayed through it in tests.
#[derive(Debug, Default)]
pub struct Assembler {
    content: String,
    reasoning: String,
    calls: BTreeMap<usize, PartialCall>,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
}

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl Assembler {
    /// Applies one SSE `data` payload. `[DONE]` yields the final `LlmEvent::Done`.
    pub fn apply(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
        if data == "[DONE]" {
            return Ok(vec![LlmEvent::Done(self.complete()?)]);
        }
        let chunk: Chunk = serde_json::from_str(data).map_err(|source| LlmError::Decode {
            data: data.to_owned(),
            source,
        })?;
        if let Some(error) = chunk.error {
            let message = error
                .get("message")
                .and_then(|message| message.as_str())
                .map_or_else(|| error.to_string(), str::to_owned);
            return Err(LlmError::Provider { message });
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
            });
        }
        let mut events = Vec::new();
        for choice in chunk.choices.unwrap_or_default() {
            if choice.index != 0 {
                continue;
            }
            if let Some(delta) = choice.delta {
                if let Some(reasoning) = delta.reasoning_content.or(delta.reasoning)
                    && !reasoning.is_empty()
                {
                    self.reasoning.push_str(&reasoning);
                    events.push(LlmEvent::ReasoningDelta(reasoning));
                }
                for text in [delta.content, delta.refusal].into_iter().flatten() {
                    if !text.is_empty() {
                        self.content.push_str(&text);
                        events.push(LlmEvent::TextDelta(text));
                    }
                }
                for call in delta.tool_calls.unwrap_or_default() {
                    let partial = self.calls.entry(call.index).or_default();
                    let id = call.id.filter(|id| !id.is_empty());
                    if partial.id.is_none() {
                        partial.id.clone_from(&id);
                    }
                    let (name, arguments) = call.function.map_or((None, None), |function| {
                        (
                            function.name.filter(|name| !name.is_empty()),
                            function.arguments,
                        )
                    });
                    if partial.name.is_none() {
                        partial.name.clone_from(&name);
                    }
                    let arguments = arguments.unwrap_or_default();
                    partial.arguments.push_str(&arguments);
                    events.push(LlmEvent::ToolCallDelta {
                        index: call.index,
                        id,
                        name,
                        arguments,
                    });
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(FinishReason::parse(&reason));
            }
        }
        Ok(events)
    }

    /// Called when the body ends without `[DONE]`. Some providers stop right after the chunk
    /// carrying `finish_reason`; without that chunk the reply was cut off.
    pub fn finish_without_done(&mut self) -> Result<Completion, LlmError> {
        if self.finish_reason.is_none() {
            return Err(LlmError::Truncated);
        }
        self.complete()
    }

    fn complete(&mut self) -> Result<Completion, LlmError> {
        let mut tool_calls = Vec::with_capacity(self.calls.len());
        for (index, call) in std::mem::take(&mut self.calls) {
            let id = call
                .id
                .ok_or(LlmError::InvalidToolCall { index, field: "id" })?;
            let name = call.name.ok_or(LlmError::InvalidToolCall {
                index,
                field: "name",
            })?;
            tool_calls.push(ToolCall {
                id,
                name,
                arguments: call.arguments,
            });
        }
        let reasoning = std::mem::take(&mut self.reasoning);
        Ok(Completion {
            content: std::mem::take(&mut self.content),
            reasoning: (!reasoning.is_empty()).then_some(reasoning),
            tool_calls,
            finish_reason: self.finish_reason.take(),
            usage: self.usage.take(),
        })
    }
}
