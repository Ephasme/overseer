//! JSON shapes exchanged with the endpoint.

use serde::Deserialize;
use serde_json::{Value, json};

use super::types::{ChatRequest, Message, ToolCall};

pub(crate) fn request_body(model: &str, request: &ChatRequest, send_reasoning: bool) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| message_json(message, send_reasoning))
        .collect();
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    body
}

fn message_json(message: &Message, send_reasoning: bool) -> Value {
    match message {
        Message::System { content } => json!({ "role": "system", "content": content }),
        Message::User { content } => json!({ "role": "user", "content": content }),
        Message::Tool {
            tool_call_id,
            content,
        } => json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content }),
        Message::Assistant {
            content,
            reasoning,
            tool_calls,
        } => {
            let mut value = json!({ "role": "assistant" });
            value["content"] = if content.is_empty() && !tool_calls.is_empty() {
                Value::Null
            } else {
                Value::String(content.clone())
            };
            if !tool_calls.is_empty() {
                value["tool_calls"] = tool_calls.iter().map(tool_call_json).collect();
            }
            if let (true, Some(reasoning)) = (send_reasoning, reasoning) {
                value["reasoning_content"] = Value::String(reasoning.clone());
            }
            value
        }
    }
}

fn tool_call_json(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": { "name": call.name, "arguments": call.arguments },
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct Chunk {
    pub choices: Option<Vec<Choice>>,
    pub usage: Option<WireUsage>,
    pub error: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    #[serde(default)]
    pub index: u32,
    pub delta: Option<Delta>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Delta {
    pub content: Option<String>,
    pub refusal: Option<String>,
    /// `DeepSeek`, `vLLM`.
    pub reasoning_content: Option<String>,
    /// `OpenRouter`.
    pub reasoning: Option<String>,
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::request_body;
    use crate::llm::types::{ChatRequest, Message, ToolCall, ToolDefinition};

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![
                Message::System {
                    content: "be brief".into(),
                },
                Message::User {
                    content: "list files".into(),
                },
                Message::Assistant {
                    content: String::new(),
                    reasoning: Some("use bash".into()),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "bash".into(),
                        arguments: r#"{"command":"ls"}"#.into(),
                    }],
                },
                Message::Tool {
                    tool_call_id: "call_1".into(),
                    content: "a.rs".into(),
                },
            ],
            tools: vec![ToolDefinition {
                name: "bash".into(),
                description: "Run a command".into(),
                parameters: json!({ "type": "object", "properties": {} }),
            }],
        }
    }

    #[test]
    fn asks_for_a_stream_with_usage() {
        let body = request_body("m", &ChatRequest::default(), false);
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn serialises_every_message_role() {
        let body = request_body("m", &request(), false);
        assert_eq!(
            body["messages"],
            json!([
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "list files" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "bash", "arguments": "{\"command\":\"ls\"}" },
                    }],
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "a.rs" },
            ])
        );
    }

    #[test]
    fn sends_tools_as_function_definitions() {
        let body = request_body("m", &request(), false);
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "Run a command",
                    "parameters": { "type": "object", "properties": {} },
                },
            }])
        );
    }

    #[test]
    fn omits_tools_when_there_are_none() {
        let body = request_body("m", &ChatRequest::default(), false);
        assert!(body.get("tools").is_none(), "{body}");
    }

    #[test]
    fn omits_reasoning_by_default() {
        let body = request_body("m", &request(), false);
        assert!(
            body["messages"][2].get("reasoning_content").is_none(),
            "{body}"
        );
    }

    #[test]
    fn sends_reasoning_as_reasoning_content_when_asked() {
        let body = request_body("m", &request(), true);
        assert_eq!(body["messages"][2]["reasoning_content"], "use bash");
    }

    #[test]
    fn keeps_text_on_an_assistant_message_with_tool_calls() {
        let request = ChatRequest {
            messages: vec![Message::Assistant {
                content: "let me look".into(),
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "c".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
            }],
            tools: vec![],
        };
        let body = request_body("m", &request, false);
        assert_eq!(body["messages"][0]["content"], "let me look");
    }
}
