use crate::agent::{INTERRUPTED, cancelled};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// The usage the provider reported for this assistant message, kept so compaction can size the
    /// conversation from the provider's own count. Never serialized: it is bookkeeping.
    #[serde(skip)]
    pub(crate) usage: Option<Usage>,
    /// The raw summary body when this message is an injected compaction summary; `content` then
    /// holds the rendered wrapper. `summary.is_some()` is the test that a message is a summary.
    #[serde(skip)]
    pub(crate) summary: Option<String>,
    /// Pictures sent with this message, by path: the request carries them base64-encoded.
    /// Skipped both ways: session files keep the text, and a temp file outlives no process.
    #[serde(skip)]
    pub(crate) images: Vec<PathBuf>,
}

impl Message {
    fn new(role: &str, content: String) -> Self {
        Self {
            role: role.into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
            summary: None,
            images: Vec::new(),
        }
    }
    pub(crate) fn system(content: String) -> Self {
        Self::new("system", content)
    }
    pub(crate) fn user(content: &str) -> Self {
        Self::new("user", content.into())
    }
    /// A user message with the pictures pasted alongside it.
    pub(crate) fn user_with_images(content: &str, images: &[PathBuf]) -> Self {
        Self { images: images.to_vec(), ..Self::user(content) }
    }
    pub(crate) fn tool(tool_call_id: &str, content: String) -> Self {
        Self { tool_call_id: Some(tool_call_id.into()), ..Self::new("tool", content) }
    }
    /// An injected compaction summary: `body` is the raw text the summarizer produced, `rendered`
    /// the wrapper sent to the model.
    pub(crate) fn summary(body: String, rendered: String) -> Self {
        Self { summary: Some(body), ..Self::new("user", rendered) }
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

/// The token counts a provider reports for one model call. Most servers spell them
/// `prompt_tokens`/`completion_tokens`, so each field takes both spellings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct Usage {
    #[serde(default, alias = "prompt_tokens")]
    input_tokens: u64,
    #[serde(default, alias = "completion_tokens")]
    output_tokens: u64,
}

impl Usage {
    pub(crate) fn counts(&self) -> (u64, u64) {
        (self.input_tokens, self.output_tokens)
    }
}

/// Turns a conversation into the next assistant message. Lets tests script the model.
pub(crate) trait Llm: Send + Sync {
    /// `cancel` is the owner's interrupt flag, read between streamed lines.
    fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<(Message, Option<Usage>)>;

    /// The same call with a ceiling on the reply, for the compaction summary. A backend without a
    /// cap ignores it.
    fn chat_capped(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        max_tokens: u64,
    ) -> Result<(Message, Option<Usage>)> {
        let _ = max_tokens;
        self.chat(messages, tools, cancel)
    }

    /// The same call, reporting pieces as they arrive. A backend without streaming reports them once.
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        on_piece: &mut dyn FnMut(Piece<'_>),
    ) -> Result<(Message, Option<Usage>)> {
        let (message, usage) = self.chat(messages, tools, cancel)?;
        report(&message, on_piece);
        Ok((message, usage))
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
    /// Whether a client asks its provider for token counts: a cost ceiling or a warning needs them.
    wants_usage: bool,
    clients: Clients,
}

impl Backend {
    pub(crate) fn new(
        base_url: &str,
        api_key: String,
        default_model: String,
        debug: Option<DebugSink>,
        wants_usage: bool,
    ) -> Result<Self> {
        // reqwest's 30 s default is shorter than a long model answer.
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(600)).build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            default_model,
            debug,
            wants_usage,
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
                self.wants_usage,
            ))
        }))
    }
}

/// One agent's client, billed to that agent: a client is shared by every agent on its model, so the
/// id cannot live on the client itself.
///
/// This is the only place a model call is accounted, `chat_capped` and the compaction summaries
/// included — the client underneath keeps no meter.
pub(crate) struct Metered {
    inner: Arc<dyn Llm>,
    bounds: Arc<crate::bounds::Bounds>,
    agent: usize,
    model: String,
}

impl Metered {
    pub(crate) fn new(inner: Arc<dyn Llm>, bounds: Arc<crate::bounds::Bounds>, agent: usize, model: String) -> Self {
        Self { inner, bounds, agent, model }
    }

    /// Bills one answer. A call that reported no counts is counted as unbilled instead of silently
    /// escaping the ceiling.
    fn account(&self, usage: Option<Usage>) {
        match usage {
            Some(usage) => self.bounds.account(self.agent, &self.model, usage.counts()),
            None if self.bounds.wants_usage() => self.bounds.note_unbilled(),
            None => {}
        }
    }
}

impl Llm for Metered {
    fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<(Message, Option<Usage>)> {
        let answer = self.inner.chat(messages, tools, cancel)?;
        self.account(answer.1);
        Ok(answer)
    }

    fn chat_capped(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        max_tokens: u64,
    ) -> Result<(Message, Option<Usage>)> {
        let answer = self.inner.chat_capped(messages, tools, cancel, max_tokens)?;
        self.account(answer.1);
        Ok(answer)
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
    /// Whether a streamed call asks for its token counts.
    wants_usage: bool,
}

impl OpenAiClient {
    pub(crate) fn new(
        http: reqwest::blocking::Client,
        base_url: &str,
        api_key: String,
        model: String,
        effort: Option<String>,
        debug: Option<DebugSink>,
        wants_usage: bool,
    ) -> Self {
        Self { http, base_url: base_url.trim_end_matches('/').to_string(), api_key, model, effort, debug, wants_usage }
    }
}

impl Llm for OpenAiClient {
    fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        _cancel: Option<&AtomicBool>,
    ) -> Result<(Message, Option<Usage>)> {
        let body = self.post(messages, tools, None, false)?;
        let (message, usage) = parse_response(&body)?;
        Ok((message, usage))
    }

    /// A summary is not shown as it streams, and a stalled read cannot be aborted in place, so the
    /// request runs on a worker while this thread polls the interrupt flag: `/stop` returns at once
    /// instead of waiting for up to 16k output tokens.
    fn chat_capped(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        max_tokens: u64,
    ) -> Result<(Message, Option<Usage>)> {
        if cancelled(cancel) {
            bail!(INTERRUPTED);
        }
        let worker = Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            debug: self.debug.clone(),
            wants_usage: self.wants_usage,
        };
        let messages = messages.to_vec();
        let tools = tools.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // The flag is read here, not on the worker: dropping the receiver is what ends this.
            let _ = sender.send(worker.exchange(&messages, &tools, None, Some(max_tokens), &mut |_| ()));
        });
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(reply) => return reply,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if cancelled(cancel) => bail!(INTERRUPTED),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("the summary request ended without an answer");
                }
            }
        }
    }

    fn chat_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        on_piece: &mut dyn FnMut(Piece<'_>),
    ) -> Result<(Message, Option<Usage>)> {
        let response = self.send(messages, tools, None, true)?;
        let streamed = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if !streamed {
            // A provider that ignored `stream` sends one whole response instead.
            let body = response.text().context("reading model response failed")?;
            let (message, usage) = parse_response(&body)?;
            report(&message, on_piece);
            return Ok((message, usage));
        }
        let (message, usage) = read_stream(std::io::BufReader::new(response), cancel, on_piece)?;
        Ok((message, usage))
    }
}

impl OpenAiClient {
    /// One request, read as a stream so the interrupt flag is read between lines. A provider that
    /// ignored `stream` answers whole instead.
    fn exchange(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        max_tokens: Option<u64>,
        on_piece: &mut dyn FnMut(Piece<'_>),
    ) -> Result<(Message, Option<Usage>)> {
        let response = self.send(messages, tools, max_tokens, true)?;
        let streamed = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if streamed {
            let (message, usage) = read_stream(std::io::BufReader::new(response), cancel, on_piece)?;
            return Ok((message, usage));
        }
        let status = response.status();
        let body = response.text().context("reading model response failed")?;
        if !status.is_success() {
            bail!("model returned HTTP {status}: {}", excerpt(&body));
        }
        let (message, usage) = parse_response(&body)?;
        report(&message, on_piece);
        Ok((message, usage))
    }

    fn post(&self, messages: &[Message], tools: &Value, max_tokens: Option<u64>, stream: bool) -> Result<String> {
        let response = self.send(messages, tools, max_tokens, stream)?;
        let status = response.status();
        let body = response.text().context("reading model response failed")?;
        if !status.is_success() {
            bail!("model returned HTTP {status}: {}", excerpt(&body));
        }
        Ok(body)
    }

    fn payload(
        &self,
        messages: &[Message],
        tools: &Value,
        max_tokens: Option<u64>,
        stream: bool,
        usage: bool,
    ) -> Result<Value> {
        let mut payload = json!({
            "model": self.model,
            "messages": wire_messages(messages)?,
            "tools": tools,
            "stream": stream,
            "n": 1,
        });
        if let Some(max_tokens) = max_tokens
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("max_tokens".into(), Value::from(max_tokens));
        }
        // Gated: some servers reject the field, and with no cost bound the tokens are not needed.
        if stream
            && usage
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("stream_options".into(), json!({ "include_usage": true }));
        }
        if let Some(effort) = &self.effort
            && let Some(object) = payload.as_object_mut()
        {
            object.insert("reasoning_effort".into(), Value::String(effort.clone()));
        }
        Ok(payload)
    }

    /// Whether this client asks its provider for token counts on streamed calls.
    fn wants_usage(&self) -> bool {
        self.wants_usage
    }

    fn send(
        &self,
        messages: &[Message],
        tools: &Value,
        max_tokens: Option<u64>,
        stream: bool,
    ) -> Result<reqwest::blocking::Response> {
        let mut payload = self.payload(messages, tools, max_tokens, stream, self.wants_usage())?;
        let sent = |payload: &Value| {
            if let Some(sink) = &self.debug {
                sink(&without_image_data(payload).to_string());
            }
            self.http
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .json(payload)
                .send()
                .context("request to model failed")
        };
        let mut response = sent(&payload)?;
        // A server that rejects `stream_options` with a 400 gets one retry without it: the answer
        // is worth streaming even if its tokens cannot be billed.
        if stream && self.wants_usage() && response.status() == reqwest::StatusCode::BAD_REQUEST {
            payload = self.payload(messages, tools, max_tokens, stream, false)?;
            response = sent(&payload)?;
        }
        let status = response.status();
        if stream && !status.is_success() {
            let body = response.text().context("reading model response failed")?;
            bail!("model returned HTTP {status}: {}", excerpt(&body));
        }
        Ok(response)
    }
}

/// The prefix every picture part carries; the clipboard writes PNG files only.
const DATA_URI_PREFIX: &str = "data:image/png;base64,";

/// The request's `messages`: a message carrying pictures widens `content` to parts, because the
/// plain-string form has no place for one.
fn wire_messages(messages: &[Message]) -> serde_json::Result<Vec<Value>> {
    messages
        .iter()
        .map(|message| {
            let mut value = serde_json::to_value(message)?;
            if !message.images.is_empty()
                && let Some(object) = value.as_object_mut()
            {
                object.insert("content".into(), Value::Array(content_parts(message)));
            }
            Ok(value)
        })
        .collect()
}

/// The text first, then one part per picture. A picture that cannot be read becomes a line of
/// text saying so: a vanished temp file costs the picture, never the turn.
fn content_parts(message: &Message) -> Vec<Value> {
    let text = message.content.as_deref().filter(|text| !text.trim().is_empty());
    let pictures = message.images.iter().map(|path| match data_uri(path) {
        Ok(url) => json!({ "type": "image_url", "image_url": { "url": url } }),
        Err(why) => json!({ "type": "text", "text": format!("[image unavailable: {}: {why}]", path.display()) }),
    });
    text.map(|text| json!({ "type": "text", "text": text })).into_iter().chain(pictures).collect()
}

fn data_uri(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(format!("{DATA_URI_PREFIX}{}", STANDARD.encode(bytes)))
}

/// The payload as the debug sink shows it: each picture's data reduced to its length, or
/// `OVERSEER_DEBUG=1` would dump megabytes on every call.
fn without_image_data(payload: &Value) -> Value {
    let mut copy = payload.clone();
    let parts = copy
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get_mut("content").and_then(Value::as_array_mut))
        .flatten();
    for part in parts {
        if let Some(url) = part.pointer_mut("/image_url/url")
            && let Some(data) = url.as_str().and_then(|url| url.strip_prefix(DATA_URI_PREFIX))
            && !data.is_empty()
        {
            *url = Value::String(format!("{DATA_URI_PREFIX}[{} base64 chars]", data.len()));
        }
    }
    copy
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

fn parse_response(body: &str) -> Result<(Message, Option<Usage>)> {
    let response: ChatResponse =
        serde_json::from_str(body).with_context(|| format!("unexpected model response: {}", excerpt(body)))?;
    response
        .choices
        .into_iter()
        .next()
        .map(|choice| (choice.message, response.usage))
        .context("model response has no choices")
}

/// One `data:` line of a chat-completions stream.
#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
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
) -> Result<(Message, Option<Usage>)> {
    let mut content = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut complete = false;
    let mut usage = None;
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
        if chunk.usage.is_some() {
            // Servers send usage on a final chunk whose `choices` is empty; the last one wins.
            usage = chunk.usage;
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
    let message = Message {
        role: "assistant".into(),
        content,
        tool_calls,
        tool_call_id: None,
        usage: None,
        summary: None,
        images: Vec::new(),
    };
    Ok((message, usage))
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
    fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
    ) -> Result<(Message, Option<Usage>)> {
        self.chat_stream(messages, tools, cancel, &mut |_| ())
    }

    fn chat_stream(
        &self,
        messages: &[Message],
        tools: &Value,
        cancel: Option<&AtomicBool>,
        on_piece: &mut dyn FnMut(Piece<'_>),
    ) -> Result<(Message, Option<Usage>)> {
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

    /// A usage a scripted provider reports, for tests that need one.
    pub(crate) fn usage(input: u64, output: u64) -> Usage {
        serde_json::from_value(json!({ "input_tokens": input, "output_tokens": output })).unwrap()
    }

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
            usage: None,
            summary: None,
            images: Vec::new(),
        }
    }

    pub(crate) fn text(content: &str) -> Message {
        Message {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
            summary: None,
            images: Vec::new(),
        }
    }

    /// Returns scripted replies in order; errors once exhausted.
    pub(crate) struct Scripted(Mutex<Vec<Message>>);

    impl Scripted {
        pub(crate) fn new(replies: Vec<Message>) -> Self {
            Self(Mutex::new(replies))
        }
    }

    impl Llm for Scripted {
        fn chat(&self, _: &[Message], _: &Value, _: Option<&AtomicBool>) -> Result<(Message, Option<Usage>)> {
            let mut replies = self.0.lock().unwrap();
            if replies.is_empty() {
                bail!("script exhausted");
            }
            Ok((replies.remove(0), None))
        }
    }

    /// One scripted client for every model and effort: a test sees no provider.
    pub(crate) struct Fixed(pub(crate) Arc<dyn Llm>);

    impl Models for Fixed {
        fn get(&self, _: Option<&str>, _: Option<&str>) -> Arc<dyn Llm> {
            Arc::clone(&self.0)
        }
    }

    /// Records every request it sees, then delegates: a test reads back what reached the model.
    pub(crate) struct Recording {
        inner: Arc<dyn Llm>,
        pub(crate) requests: Mutex<Vec<Vec<Message>>>,
    }

    impl Recording {
        pub(crate) fn new(inner: Arc<dyn Llm>) -> Self {
            Self { inner, requests: Mutex::new(Vec::new()) }
        }
    }

    impl Llm for Recording {
        fn chat(
            &self,
            messages: &[Message],
            tools: &Value,
            cancel: Option<&AtomicBool>,
        ) -> Result<(Message, Option<Usage>)> {
            self.requests.lock().unwrap().push(messages.to_vec());
            self.inner.chat(messages, tools, cancel)
        }

        fn chat_stream(
            &self,
            messages: &[Message],
            tools: &Value,
            cancel: Option<&AtomicBool>,
            on_piece: &mut dyn FnMut(Piece<'_>),
        ) -> Result<(Message, Option<Usage>)> {
            self.requests.lock().unwrap().push(messages.to_vec());
            self.inner.chat_stream(messages, tools, cancel, on_piece)
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
        let (message, _) = read_stream(std::io::Cursor::new(sse), None, &mut |piece| {
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
        let (message, _) = read_stream(std::io::Cursor::new(sse), None, &mut |_| ()).unwrap();
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
        let (message, _) = read_stream(std::io::Cursor::new(sse), None, &mut |piece| {
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
        let (m, _) =
            parse_response(r#"{"choices":[{"message":{"role":"assistant","content":"hello","tool_calls":null}}]}"#)
                .unwrap();
        assert_eq!(m.content.as_deref(), Some("hello"));
        assert_eq!(m.tool_calls, None);
    }

    #[test]
    fn parses_tool_call_reply() {
        let (m, _) = parse_response(r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}}]}"#).unwrap();
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
        let plain = OpenAiClient::new(http.clone(), "http://x/v1", "k".into(), "m".into(), None, None, true);
        let body = plain.payload(&[], &json!([]), None, false, false).unwrap();
        assert!(body.get("reasoning_effort").is_none());
        let effortful =
            OpenAiClient::new(http, "http://x/v1", "k".into(), "m".into(), Some("high".into()), None, false);
        let body = effortful.payload(&[], &json!([]), None, false, false).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["model"], "m");
    }

    fn client() -> OpenAiClient {
        OpenAiClient::new(reqwest::blocking::Client::new(), "http://x/v1", "k".into(), "m".into(), None, None, false)
    }

    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

    #[test]
    fn an_image_rides_a_message_as_a_data_uri_part() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, PNG_MAGIC).unwrap();
        let body =
            client().payload(&[Message::user_with_images("what is this?", &[png])], &json!([]), None, false, false);
        let content = &body.unwrap()["messages"][0]["content"];
        assert_eq!(content[0], json!({ "type": "text", "text": "what is this?" }));
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], format!("data:image/png;base64,{}", STANDARD.encode(PNG_MAGIC)));
    }

    #[test]
    fn a_message_without_a_picture_keeps_its_text_content() {
        let body = client().payload(&[Message::user("hi")], &json!([]), None, false, false).unwrap();
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn a_picture_that_vanished_becomes_a_line_of_text() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone.png");
        let body = client().payload(&[Message::user_with_images("look", &[gone])], &json!([]), None, false, false);
        let content = &body.unwrap()["messages"][0]["content"];
        assert_eq!(content[0]["text"], "look");
        assert!(content[1]["text"].as_str().unwrap().starts_with("[image unavailable:"), "{content}");
    }

    #[test]
    fn the_debug_copy_hides_the_image_data() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.png");
        std::fs::write(&png, PNG_MAGIC).unwrap();
        let body = client().payload(&[Message::user_with_images("x", &[png])], &json!([]), None, false, false).unwrap();
        let shown = without_image_data(&body);
        let encoded = STANDARD.encode(PNG_MAGIC).len();
        assert_eq!(
            shown["messages"][0]["content"][1]["image_url"]["url"],
            format!("data:image/png;base64,[{encoded} base64 chars]")
        );
        assert_eq!(shown["messages"][0]["content"][0], body["messages"][0]["content"][0]);
    }

    #[test]
    fn the_backend_reuses_one_client_per_pair() {
        let backend = Backend::new("http://x/v1", "k".into(), "default".into(), None, false).unwrap();
        let first = backend.get(None, None);
        assert!(Arc::ptr_eq(&first, &backend.get(None, None)), "the default pair is built once");
        assert!(!Arc::ptr_eq(&first, &backend.get(Some("other"), None)));
        assert!(!Arc::ptr_eq(&first, &backend.get(None, Some("high"))));
    }

    #[test]
    fn a_response_carries_the_provider_usage() {
        let (m, usage) = parse_response(
            r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
        )
        .unwrap();
        assert_eq!(m.content.as_deref(), Some("hi"));
        assert_eq!(usage, Some(Usage { input_tokens: 7, output_tokens: 3 }));
        // A response without one reports no usage, and the client bills nothing.
        let (_, none) = parse_response(r#"{"choices":[{"message":{"role":"assistant","content":"hi"}}]}"#).unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn a_streams_final_usage_chunk_reaches_the_caller() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let (message, usage) = read_stream(std::io::Cursor::new(sse), None, &mut |_| ()).unwrap();
        assert_eq!(message.content.as_deref(), Some("hi"));
        assert_eq!(usage, Some(Usage { input_tokens: 5, output_tokens: 2 }));
    }

    #[test]
    fn the_stream_options_key_appears_only_for_a_cost_bound() {
        let http = reqwest::blocking::Client::new();
        let bounded = crate::bounds::Bounds::new(None, Some(1.0), HashMap::new());
        let client =
            OpenAiClient::new(http.clone(), "http://x/v1", "k".into(), "m".into(), None, None, bounded.wants_usage());
        assert!(client.payload(&[], &json!([]), None, true, true).unwrap().get("stream_options").is_some());
        assert!(
            client.payload(&[], &json!([]), None, false, true).unwrap().get("stream_options").is_none(),
            "non-streaming asks for usage inline"
        );
        let unbounded = OpenAiClient::new(http, "http://x/v1", "k".into(), "m".into(), None, None, false);
        assert!(unbounded.payload(&[], &json!([]), None, true, false).unwrap().get("stream_options").is_none());
    }

    /// A client that answers with whatever token counts it was built with.
    struct Counted(Option<Usage>);

    impl Llm for Counted {
        fn chat(&self, _: &[Message], _: &Value, _: Option<&AtomicBool>) -> Result<(Message, Option<Usage>)> {
            Ok((testing::text("hi"), self.0))
        }
    }

    #[test]
    fn a_priced_model_is_billed_to_its_agent_and_an_unpriced_one_is_not() {
        let pricing = HashMap::from([("m".into(), crate::bounds::Price { input: 3.0, output: 15.0 })]);
        let meter = Arc::new(crate::bounds::Bounds::new(None, Some(100.0), pricing));
        let million = Usage { input_tokens: 1_000_000, output_tokens: 1_000_000 };
        let metered = Metered::new(Arc::new(Counted(Some(million))), Arc::clone(&meter), 2, "m".into());
        metered.chat(&[], &json!([]), None).unwrap();
        assert_eq!(meter.spent(), 18.0);
        assert_eq!(meter.agents(), [(2, crate::bounds::Spend { input: 1_000_000, output: 1_000_000, cost: 18.0 })]);
        let unpriced = Metered::new(Arc::new(Counted(Some(million))), Arc::clone(&meter), 3, "unpriced".into());
        unpriced.chat(&[], &json!([]), None).unwrap();
        assert_eq!(meter.spent(), 18.0, "a model the table does not name counts $0");
        assert_eq!(meter.tokens(), 4_000_000, "its tokens are counted all the same");
    }

    #[test]
    fn a_call_that_reported_no_counts_is_counted_as_unbilled() {
        let meter = Arc::new(crate::bounds::Bounds::new(None, Some(1.0), HashMap::new()));
        let metered = Metered::new(Arc::new(Counted(None)), Arc::clone(&meter), 1, "m".into());
        metered.chat(&[], &json!([]), None).unwrap();
        assert_eq!(meter.unbilled(), 1);
        assert_eq!(meter.spent(), 0.0);
    }
}
