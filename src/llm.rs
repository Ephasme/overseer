use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// One chat message in the chat-completions wire format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Message {
    pub(crate) role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
}

impl Message {
    fn new(role: &str, content: String) -> Self {
        Self { role: role.into(), content: Some(content), tool_calls: None, tool_call_id: None }
    }
    pub(crate) fn system(content: String) -> Self {
        Self::new("system", content)
    }
    pub(crate) fn user(content: &str) -> Self {
        Self::new("user", content.into())
    }
    pub(crate) fn tool(tool_call_id: &str, content: String) -> Self {
        Self { tool_call_id: Some(tool_call_id.into()), ..Self::new("tool", content) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub(crate) kind: String,
    pub(crate) function: FunctionCall,
}

fn function_kind() -> String {
    "function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct FunctionCall {
    pub(crate) name: String,
    /// JSON-encoded arguments, as the API sends them.
    pub(crate) arguments: String,
}

/// Turns a conversation into the next assistant message. Lets tests script the model.
pub(crate) trait Llm: Send + Sync {
    fn chat(&self, messages: &[Message], tools: &Value) -> Result<Message>;
}

#[derive(Debug)]
pub(crate) struct OpenAiClient {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiClient {
    pub(crate) fn new(base_url: &str, api_key: String, model: String) -> Result<Self> {
        // reqwest's 30 s default is shorter than a long model answer.
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(600)).build()?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_string(), api_key, model })
    }
}

impl Llm for OpenAiClient {
    fn chat(&self, messages: &[Message], tools: &Value) -> Result<Message> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&json!({ "model": self.model, "messages": messages, "tools": tools }))
            .send()
            .context("request to model failed")?;
        let status = response.status();
        let body = response.text().context("reading model response failed")?;
        if !status.is_success() {
            bail!("model returned HTTP {status}: {body}");
        }
        parse_response(&body)
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

fn parse_response(body: &str) -> Result<Message> {
    let response: ChatResponse =
        serde_json::from_str(body).with_context(|| format!("unexpected model response: {body}"))?;
    response.choices.into_iter().next().map(|c| c.message).context("model response has no choices")
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    pub(crate) fn tool_call(id: &str, name: &str, args: &Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall { name: name.into(), arguments: args.to_string() },
        }
    }

    pub(crate) fn call(id: &str, name: &str, args: &Value) -> Message {
        Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![tool_call(id, name, args)]),
            tool_call_id: None,
        }
    }

    pub(crate) fn text(content: &str) -> Message {
        Message { role: "assistant".into(), content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }

    /// Returns scripted replies in order; errors once exhausted.
    pub(crate) struct Scripted(Mutex<Vec<Message>>);

    impl Scripted {
        pub(crate) fn new(replies: Vec<Message>) -> Self {
            Self(Mutex::new(replies))
        }
    }

    impl Llm for Scripted {
        fn chat(&self, _: &[Message], _: &Value) -> Result<Message> {
            let mut replies = self.0.lock().unwrap();
            if replies.is_empty() {
                bail!("script exhausted");
            }
            Ok(replies.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_reply_with_null_tool_calls() {
        let m = parse_response(r#"{"choices":[{"message":{"role":"assistant","content":"hello","tool_calls":null}}]}"#)
            .unwrap();
        assert_eq!(m.content.as_deref(), Some("hello"));
        assert_eq!(m.tool_calls, None);
    }

    #[test]
    fn parses_tool_call_reply() {
        let m = parse_response(r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#).unwrap();
        let calls = m.tool_calls.unwrap();
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn rejects_empty_choices() {
        assert!(parse_response(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn tool_message_omits_absent_fields() {
        // Some providers reject `"tool_calls": null` on non-assistant messages.
        let v = serde_json::to_value(Message::tool("c1", "out".into())).unwrap();
        assert_eq!(v, json!({"role":"tool","content":"out","tool_call_id":"c1"}));
    }
}
