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

/// The only tool-call type the chat-completions API defines.
const FUNCTION_KIND: &str = "function";

fn function_kind() -> String {
    FUNCTION_KIND.into()
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

    /// The same call, reporting content as it arrives. A backend without streaming reports it once.
    fn chat_stream(&self, messages: &[Message], tools: &Value, on_delta: &mut dyn FnMut(&str)) -> Result<Message> {
        let message = self.chat(messages, tools)?;
        on_delta(message.content.as_deref().unwrap_or_default());
        Ok(message)
    }
}

/// Where `OVERSEER_DEBUG` output goes: stderr in line mode, the transcript in the TUI.
pub(crate) type DebugSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub(crate) struct OpenAiClient {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    model: String,
    debug: Option<DebugSink>,
}

impl OpenAiClient {
    pub(crate) fn new(base_url: &str, api_key: String, model: String, debug: Option<DebugSink>) -> Result<Self> {
        // reqwest's 30 s default is shorter than a long model answer.
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(600)).build()?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_string(), api_key, model, debug })
    }
}

impl Llm for OpenAiClient {
    fn chat(&self, messages: &[Message], tools: &Value) -> Result<Message> {
        let body = self.post(messages, tools, false)?;
        parse_response(&body)
    }

    fn chat_stream(&self, messages: &[Message], tools: &Value, on_delta: &mut dyn FnMut(&str)) -> Result<Message> {
        let response = self.send(messages, tools, true)?;
        let streamed = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if !streamed {
            // A provider that ignored `stream` sends one whole response instead.
            let body = response.text().context("reading model response failed")?;
            let message = parse_response(&body)?;
            on_delta(message.content.as_deref().unwrap_or_default());
            return Ok(message);
        }
        read_stream(std::io::BufReader::new(response), on_delta)
    }
}

impl OpenAiClient {
    fn post(&self, messages: &[Message], tools: &Value, stream: bool) -> Result<String> {
        let response = self.send(messages, tools, stream)?;
        let status = response.status();
        let body = response.text().context("reading model response failed")?;
        if !status.is_success() {
            bail!("model returned HTTP {status}: {body}");
        }
        Ok(body)
    }

    fn send(&self, messages: &[Message], tools: &Value, stream: bool) -> Result<reqwest::blocking::Response> {
        let payload = json!({ "model": self.model, "messages": messages, "tools": tools, "stream": stream });
        if let Some(sink) = &self.debug {
            sink(&payload.to_string());
        }
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .context("request to model failed")?;
        let status = response.status();
        if stream && !status.is_success() {
            let body = response.text().context("reading model response failed")?;
            bail!("model returned HTTP {status}: {body}");
        }
        Ok(response)
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

/// One `data:` line of a chat-completions stream.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    tool_calls: Option<Vec<Fragment>>,
}

/// A piece of one tool call: the API splits arguments across chunks, naming the call by `index`.
#[derive(Deserialize)]
struct Fragment {
    index: usize,
    id: Option<String>,
    function: Option<FragmentFunction>,
}

#[derive(Deserialize)]
struct FragmentFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// Rebuilds the assistant message from `data:` lines, reporting each content delta as it arrives.
fn read_stream(reader: impl std::io::BufRead, on_delta: &mut dyn FnMut(&str)) -> Result<Message> {
    let mut content = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    for line in reader.lines() {
        let line = line.context("reading model stream failed")?;
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        let chunk: StreamChunk =
            serde_json::from_str(data).with_context(|| format!("unexpected stream chunk: {data}"))?;
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content {
                content.push_str(&text);
                on_delta(&text);
            }
            for fragment in choice.delta.tool_calls.unwrap_or_default() {
                merge_fragment(&mut calls, fragment);
            }
        }
    }
    let tool_calls = (!calls.is_empty()).then_some(calls);
    let content = (!content.is_empty()).then_some(content);
    Ok(Message { role: "assistant".into(), content, tool_calls, tool_call_id: None })
}

fn merge_fragment(calls: &mut Vec<ToolCall>, fragment: Fragment) {
    while calls.len() <= fragment.index {
        calls.push(ToolCall {
            id: String::new(),
            kind: FUNCTION_KIND.into(),
            function: FunctionCall { name: String::new(), arguments: String::new() },
        });
    }
    let Some(call) = calls.get_mut(fragment.index) else {
        return;
    };
    if let Some(id) = fragment.id {
        call.id = id;
    }
    if let Some(function) = fragment.function {
        if call.function.name.is_empty() {
            call.function.name = function.name.unwrap_or_default();
        }
        if let Some(arguments) = function.arguments {
            call.function.arguments.push_str(&arguments);
        }
    }
}

/// Forwards the model's streaming text to a sink while `Turn` keeps its blocking interface.
pub(crate) struct StreamLlm {
    inner: std::sync::Arc<dyn Llm>,
    sink: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
}

impl StreamLlm {
    pub(crate) fn new(inner: std::sync::Arc<dyn Llm>, sink: std::sync::Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Self { inner, sink }
    }
}

impl Llm for StreamLlm {
    fn chat(&self, messages: &[Message], tools: &Value) -> Result<Message> {
        // A turn makes several model calls; the frontend needs to see where one answer ends.
        (self.sink)("\n");
        self.inner.chat_stream(messages, tools, &mut |delta| (self.sink)(delta))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    pub(crate) fn tool_call(id: &str, name: &str, args: &Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: FUNCTION_KIND.into(),
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
    fn stream_reassembles_text_and_split_tool_calls() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Looking \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"now.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
            "\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"comm\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"and\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut seen = String::new();
        let message = read_stream(std::io::Cursor::new(sse), &mut |delta| seen.push_str(delta)).unwrap();
        assert_eq!(seen, "Looking now.");
        assert_eq!(message.content.as_deref(), Some("Looking now."));
        let calls = message.tool_calls.unwrap();
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn stream_ignores_usage_chunks_and_blank_lines() {
        let sse = "data: {\"choices\":[],\"usage\":{\"total_tokens\":3}}\n\ndata: [DONE]\n\n";
        let message = read_stream(std::io::Cursor::new(sse), &mut |_| ()).unwrap();
        assert_eq!(message.content, None);
        assert_eq!(message.tool_calls, None);
    }
    #[test]
    fn stream_wrapper_marks_each_model_call() {
        let inner: std::sync::Arc<dyn Llm> = std::sync::Arc::new(testing::Scripted::new(vec![testing::text("hi")]));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = {
            let seen = std::sync::Arc::clone(&seen);
            std::sync::Arc::new(move |delta: &str| seen.lock().unwrap().push_str(delta))
        };
        let llm = StreamLlm::new(inner, sink);
        llm.chat(&[], &json!([])).unwrap();
        assert_eq!(*seen.lock().unwrap(), "\nhi");
    }

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
