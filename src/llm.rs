use crate::agent::{INTERRUPTED, cancelled};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Longest response body echoed into an error: a proxy's error page must not land in the transcript.
const MAX_ERROR_BODY: usize = 2_000;

fn excerpt(body: &str) -> String {
    if body.len() <= MAX_ERROR_BODY {
        return body.to_string();
    }
    let mut end = MAX_ERROR_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", body.get(..end).unwrap_or_default())
}

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

/// One piece of a streamed model call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Piece<'a> {
    /// The answer: shown to the user and kept in the history.
    Text(&'a str),
    /// The model's reasoning: shown to the user, never kept.
    Reasoning(&'a str),
}

/// Turns a conversation into the next assistant message. Lets tests script the model.
pub(crate) trait Llm: Send + Sync {
    /// `cancel` is the owner's interrupt flag, read between streamed lines.
    fn chat(&self, messages: &[Message], tools: &Value, cancel: Option<&AtomicBool>) -> Result<Message>;

    /// The same call, reporting pieces as they arrive. A backend without streaming reports them once.
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        on_piece: &mut dyn FnMut(Piece<'_>),
    ) -> Result<Message> {
        let message = self.chat(messages, tools, cancel)?;
        report(&message, on_piece);
        Ok(message)
    }
}

/// Reports a whole message as the piece streaming it would have produced.
fn report(message: &Message, on_piece: &mut dyn FnMut(Piece<'_>)) {
    if let Some(content) = message.content.as_deref().filter(|text| !text.is_empty()) {
        on_piece(Piece::Text(content));
    }
}

/// Where `OVERSEER_DEBUG` output goes: stderr in line mode, the transcript in the TUI.
pub(crate) type DebugSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Builds the client for one agent: a backend's default, or the model and effort a profile names.
pub(crate) trait Models: Send + Sync {
    fn get(&self, model: Option<&str>, effort: Option<&str>) -> Arc<dyn Llm>;
}

/// One client per `(model, effort)` pair, shared rather than rebuilt: a pair costs a pool.
type Clients = Mutex<HashMap<(String, Option<String>), Arc<dyn Llm>>>;

/// The real backend: one cheap client per `(model, effort)` pair, shared by every agent that asks.
pub(crate) struct Backend {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    default_model: String,
    debug: Option<DebugSink>,
    clients: Clients,
}

impl Backend {
    pub(crate) fn new(
        base_url: &str,
        api_key: String,
        default_model: String,
        debug: Option<DebugSink>,
    ) -> Result<Self> {
        // reqwest's 30 s default is shorter than a long model answer.
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(600)).build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            default_model,
            debug,
            clients: Mutex::new(HashMap::new()),
        })
    }
}

impl Models for Backend {
    fn get(&self, model: Option<&str>, effort: Option<&str>) -> Arc<dyn Llm> {
        let model = model.unwrap_or(&self.default_model).to_string();
        let key = (model.clone(), effort.map(str::to_string));
        let mut clients = self.clients.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(clients.entry(key).or_insert_with(move || {
            Arc::new(OpenAiClient::new(
                self.http.clone(),
                &self.base_url,
                self.api_key.clone(),
                model,
                effort.map(str::to_string),
                self.debug.clone(),
            ))
        }))
    }
}

pub(crate) struct OpenAiClient {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Sent as `reasoning_effort`; `None` leaves the provider's own default.
    effort: Option<String>,
    debug: Option<DebugSink>,
}

impl OpenAiClient {
    pub(crate) fn new(
        http: reqwest::blocking::Client,
        base_url: &str,
        api_key: String,
        model: String,
        effort: Option<String>,
        debug: Option<DebugSink>,
    ) -> Self {
        Self { http, base_url: base_url.trim_end_matches('/').to_string(), api_key, model, effort, debug }
    }
}

impl Llm for OpenAiClient {
    fn chat(&self, messages: &[Message], tools: &Value, _cancel: Option<&AtomicBool>) -> Result<Message> {
        let body = self.post(messages, tools, false)?;
        parse_response(&body)
    }

    fn chat_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        on_piece: &mut dyn FnMut(Piece<'_>),
    ) -> Result<Message> {
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
            report(&message, on_piece);
            return Ok(message);
        }
        read_stream(std::io::BufReader::new(response), cancel, on_piece)
    }
}

impl OpenAiClient {
    fn post(&self, messages: &[Message], tools: &Value, stream: bool) -> Result<String> {
        let response = self.send(messages, tools, stream)?;
        let status = response.status();
        let body = response.text().context("reading model response failed")?;
        if !status.is_success() {
            bail!("model returned HTTP {status}: {}", excerpt(&body));
        }
        Ok(body)
    }

    fn payload(&self, messages: &[Message], tools: &Value, stream: bool) -> Value {
        let mut payload =
            json!({ "model": self.model, "messages": messages, "tools": tools, "stream": stream, "n": 1 });
        if let Some(effort) = &self.effort
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("reasoning_effort".into(), Value::String(effort.clone()));
        }
        payload
    }

    fn send(&self, messages: &[Message], tools: &Value, stream: bool) -> Result<reqwest::blocking::Response> {
        let payload = self.payload(messages, tools, stream);
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
            bail!("model returned HTTP {status}: {}", excerpt(&body));
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
        serde_json::from_str(body).with_context(|| format!("unexpected model response: {}", excerpt(body)))?;
    response.choices.into_iter().next().map(|c| c.message).context("model response has no choices")
}

/// One `data:` line of a chat-completions stream.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// A provider reports a mid-stream failure here, with no choices.
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    /// Which of the request's `n` answers this chunk belongs to; only the first is kept.
    #[serde(default)]
    index: usize,
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    /// The reasoning a thinking model streams before its answer. Providers disagree on the name,
    /// and one of them sends both keys with the same text, so each is read into its own field.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
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

/// Rebuilds the assistant message from `data:` lines, reporting each piece as it arrives.
fn read_stream(
    reader: impl std::io::BufRead,
    cancel: Option<&AtomicBool>,
    on_piece: &mut dyn FnMut(Piece<'_>),
) -> Result<Message> {
    let mut content = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut complete = false;
    for line in reader.lines() {
        if cancelled(cancel) {
            bail!(INTERRUPTED);
        }
        let line = line.context("reading model stream failed")?;
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            complete = true;
            break;
        }
        let chunk: StreamChunk =
            serde_json::from_str(data).with_context(|| format!("unexpected stream chunk: {}", excerpt(data)))?;
        if let Some(error) = chunk.error {
            bail!("model stream failed: {error}");
        }
        for choice in chunk.choices {
            if choice.index != 0 {
                continue;
            }
            if choice.finish_reason.is_some() {
                complete = true;
            }
            let Delta { content: text, reasoning_content, reasoning: alias, tool_calls } = choice.delta;
            // A provider sends an empty `content` beside each reasoning delta and an empty
            // `reasoning` beside the answer; an empty piece would open an entry of its own.
            if let Some(thought) = reasoning_content.or(alias).filter(|text| !text.is_empty()) {
                on_piece(Piece::Reasoning(&thought));
            }
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                on_piece(Piece::Text(&text));
                content.push_str(&text);
            }
            for fragment in tool_calls.unwrap_or_default() {
                merge_fragment(&mut calls, fragment)?;
            }
        }
    }
    if !complete {
        bail!("model stream ended before it finished");
    }
    if calls.is_empty() && content.is_empty() {
        bail!("model returned an empty answer");
    }
    for (index, call) in calls.iter().enumerate() {
        if call.id.is_empty() {
            bail!("tool call {index} has no id: the stream was truncated");
        }
        if call.function.name.is_empty() {
            bail!("tool call {index} has no function name: the stream was truncated");
        }
    }
    let tool_calls = (!calls.is_empty()).then_some(calls);
    let content = (!content.is_empty()).then_some(content);
    Ok(Message { role: "assistant".into(), content, tool_calls, tool_call_id: None })
}

fn merge_fragment(calls: &mut Vec<ToolCall>, fragment: Fragment) -> Result<()> {
    if fragment.index > calls.len() {
        bail!("tool call {} arrived before tool call {}", fragment.index, calls.len());
    }
    if fragment.index == calls.len() {
        calls.push(ToolCall {
            id: String::new(),
            kind: FUNCTION_KIND.into(),
            function: FunctionCall { name: String::new(), arguments: String::new() },
        });
    }
    let Some(call) = calls.get_mut(fragment.index) else {
        bail!("tool call index {} is out of range", fragment.index);
    };
    if let Some(id) = fragment.id {
        call.id = id;
    }
    if let Some(function) = fragment.function {
        if let Some(name) = function.name {
            call.function.name.push_str(&name);
        }
        if let Some(arguments) = function.arguments {
            call.function.arguments.push_str(&arguments);
        }
    }
    Ok(())
}

/// One step of a stream: a new model call, or a piece of the current one.
pub(crate) enum StreamStep<'a> {
    Start,
    Text(&'a str),
    Reasoning(&'a str),
}

/// Forwards the model's streaming output to a sink while `Turn` keeps its blocking interface.
pub(crate) struct StreamLlm {
    inner: std::sync::Arc<dyn Llm>,
    sink: std::sync::Arc<dyn Fn(StreamStep<'_>) + Send + Sync>,
}

impl StreamLlm {
    pub(crate) fn new(
        inner: std::sync::Arc<dyn Llm>,
        sink: std::sync::Arc<dyn Fn(StreamStep<'_>) + Send + Sync>,
    ) -> Self {
        Self { inner, sink }
    }
}

impl Llm for StreamLlm {
    fn chat(&self, messages: &[Message], tools: &Value, cancel: Option<&AtomicBool>) -> Result<Message> {
        self.chat_stream(messages, tools, cancel, &mut |_| ())
    }

    fn chat_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        on_piece: &mut dyn FnMut(Piece<'_>),
    ) -> Result<Message> {
        // A turn makes several model calls; the frontend needs to see where one answer ends.
        (self.sink)(StreamStep::Start);
        self.inner.chat_stream(messages, tools, cancel, &mut |piece| {
            (self.sink)(match piece {
                Piece::Text(text) => StreamStep::Text(text),
                Piece::Reasoning(text) => StreamStep::Reasoning(text),
            });
            on_piece(piece);
        })
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
        fn chat(&self, _: &[Message], _: &Value, _: Option<&AtomicBool>) -> Result<Message> {
            let mut replies = self.0.lock().unwrap();
            if replies.is_empty() {
                bail!("script exhausted");
            }
            Ok(replies.remove(0))
        }
    }

    /// One scripted client for every model and effort: a test sees no provider.
    pub(crate) struct Fixed(pub(crate) Arc<dyn Llm>);

    impl Models for Fixed {
        fn get(&self, _: Option<&str>, _: Option<&str>) -> Arc<dyn Llm> {
            Arc::clone(&self.0)
        }
    }

    /// A scripted client that also records every message list it is called with: a test asserts on
    /// the history an agent actually sent.
    pub(crate) struct Recording {
        /// What every call so far received, in order.
        pub(crate) seen: Mutex<Vec<Vec<Message>>>,
        script: Scripted,
    }

    impl Recording {
        pub(crate) fn new(replies: Vec<Message>) -> Self {
            Self { seen: Mutex::new(Vec::new()), script: Scripted::new(replies) }
        }

        /// What every call so far received, in order.
        pub(crate) fn calls(&self) -> Vec<Vec<Message>> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Llm for Recording {
        fn chat(&self, messages: &[Message], tools: &Value, cancel: Option<&AtomicBool>) -> Result<Message> {
            self.seen.lock().unwrap().push(messages.to_vec());
            self.script.chat(messages, tools, cancel)
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
        let message = read_stream(std::io::Cursor::new(sse), None, &mut |piece| {
            if let Piece::Text(text) = piece {
                seen.push_str(text);
            }
        })
        .unwrap();
        assert_eq!(seen, "Looking now.");
        assert_eq!(message.content.as_deref(), Some("Looking now."));
        let calls = message.tool_calls.unwrap();
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].function.name, "bash");
        assert_eq!(calls[0].function.arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn stream_reassembles_a_split_tool_name() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
            "\"function\":{\"name\":\"read_\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"name\":\"file\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let message = read_stream(std::io::Cursor::new(sse), None, &mut |_| ()).unwrap();
        assert_eq!(message.tool_calls.unwrap()[0].function.name, "read_file");
    }

    #[test]
    fn stream_rejects_a_tool_call_without_a_name() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
            "\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let err = read_stream(std::io::Cursor::new(sse), None, &mut |_| ()).unwrap_err();
        assert!(err.to_string().contains("no function name"), "{err}");
    }

    #[test]
    fn stream_wrapper_does_not_report_text_twice() {
        let inner: std::sync::Arc<dyn Llm> = std::sync::Arc::new(testing::Scripted::new(vec![testing::text("hi")]));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let llm = StreamLlm::new(
            inner,
            std::sync::Arc::new({
                let seen = std::sync::Arc::clone(&seen);
                move |step: StreamStep<'_>| {
                    seen.lock().unwrap().push_str(match step {
                        StreamStep::Text(text) | StreamStep::Reasoning(text) => text,
                        StreamStep::Start => "|",
                    });
                }
            }),
        );
        let mut reported = String::new();
        llm.chat_stream(&[], &json!([]), None, &mut |piece| {
            if let Piece::Text(text) = piece {
                reported.push_str(text);
            }
        })
        .unwrap();
        assert_eq!(reported, "hi");
        assert_eq!(*seen.lock().unwrap(), "|hi");
    }

    #[test]
    fn reasoning_streams_before_the_answer_and_never_goes_back() {
        // Chunk shapes copied from the provider this project runs against: it sends both reasoning
        // keys with the same text, and an empty `content` beside them.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"reasoning\":\"weigh it\",",
            "\"reasoning_content\":\"weigh it\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"reasoning\":\" twice\",",
            "\"reasoning_content\":\" twice\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\",\"role\":\"assistant\",\"reasoning\":null}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut seen = Vec::new();
        let message = read_stream(std::io::Cursor::new(sse), None, &mut |piece| {
            seen.push(match piece {
                Piece::Text(text) => format!("text {text}"),
                Piece::Reasoning(text) => format!("think {text}"),
            });
        })
        .unwrap();
        assert_eq!(seen, ["think weigh it", "think  twice", "text done"]);
        assert_eq!(message.content.as_deref(), Some("done"), "the answer holds only the answer");
        assert_eq!(message.tool_calls, None);
    }

    #[test]
    fn stream_without_text_or_tools_is_an_error() {
        let sse = "data: {\"choices\":[],\"usage\":{\"total_tokens\":3}}\n\ndata: [DONE]\n\n";
        assert!(read_stream(std::io::Cursor::new(sse), None, &mut |_| ()).is_err());
    }
    #[test]
    fn stream_reports_a_provider_error_chunk() {
        let sse = "data: {\"error\":{\"message\":\"overloaded\"}}\n\n";
        let err = read_stream(std::io::Cursor::new(sse), None, &mut |_| ()).unwrap_err();
        assert!(err.to_string().contains("overloaded"), "{err}");
    }

    #[test]
    fn stream_truncated_before_finish_is_an_error() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n";
        let err = read_stream(std::io::Cursor::new(sse), None, &mut |_| ()).unwrap_err();
        assert!(err.to_string().contains("before it finished"), "{err}");
    }

    #[test]
    fn a_cancelled_stream_stops_at_the_next_line() {
        let cancel = AtomicBool::new(false);
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut seen = String::new();
        let err = read_stream(std::io::Cursor::new(sse), Some(&cancel), &mut |piece| {
            if let Piece::Text(text) = piece {
                seen.push_str(text);
            }
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .unwrap_err();
        assert_eq!(err.to_string(), INTERRUPTED);
        assert_eq!(seen, "one", "the rest of the answer is never read");
    }

    #[test]
    fn stream_wrapper_marks_each_model_call() {
        let inner: std::sync::Arc<dyn Llm> = std::sync::Arc::new(testing::Scripted::new(vec![testing::text("hi")]));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let llm = StreamLlm::new(
            inner,
            std::sync::Arc::new({
                let seen = std::sync::Arc::clone(&seen);
                move |step: StreamStep<'_>| {
                    let text = match step {
                        StreamStep::Text(text) | StreamStep::Reasoning(text) => text,
                        StreamStep::Start => "|",
                    };
                    seen.lock().unwrap().push_str(text);
                }
            }),
        );
        llm.chat(&[], &json!([]), None).unwrap();
        assert_eq!(*seen.lock().unwrap(), "|hi");
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

    #[test]
    fn a_request_carries_the_effort_only_when_one_is_set() {
        let http = reqwest::blocking::Client::new();
        let plain = OpenAiClient::new(http.clone(), "http://x/v1", "k".into(), "m".into(), None, None);
        let body = plain.payload(&[], &json!([]), false);
        assert!(body.get("reasoning_effort").is_none());
        let effortful = OpenAiClient::new(http, "http://x/v1", "k".into(), "m".into(), Some("high".into()), None);
        let body = effortful.payload(&[], &json!([]), false);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["model"], "m");
    }

    #[test]
    fn the_backend_reuses_one_client_per_pair() {
        let backend = Backend::new("http://x/v1", "k".into(), "default".into(), None).unwrap();
        let first = backend.get(None, None);
        assert!(Arc::ptr_eq(&first, &backend.get(None, None)), "the default pair is built once");
        assert!(!Arc::ptr_eq(&first, &backend.get(Some("other"), None)));
        assert!(!Arc::ptr_eq(&first, &backend.get(None, Some("high"))));
    }
}
