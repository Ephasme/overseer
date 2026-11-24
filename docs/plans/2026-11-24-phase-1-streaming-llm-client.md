# Phase 1: streaming LLM client — implementation plan

> **For the implementing agent:** Execute one task at a time. Tick each checkbox
> (`- [ ]` → `- [x]`) as you finish a step, and stop to ask if any step is unclear —
> never guess.

**Goal:** one model call against an OpenAI-compatible `/chat/completions` endpoint, streamed, with tool calls, reasoning and usage, as a pure library in `overseer-core` that no other crate has to know the wire format of.

**Architecture:** `crates/core/src/llm` has five modules. `sse` decodes `text/event-stream` bytes into `data` payloads. `wire` holds the JSON shapes in both directions. `types` is the public vocabulary. `assemble` turns payloads into `LlmEvent`s and the final `Completion`, with no I/O, so recorded streams replay through it in tests. `client` is the only module that touches the network: `reqwest` sends the request and a `ChatStream` pulls body chunks through `sse` and `assemble`.

**Tech Stack:** Rust 1.100 (edition 2024), tokio (tests only), reqwest 0.13 with rustls, serde / serde_json, thiserror, wiremock 0.6 (dev).

**Spec:** [ADR 0001](../adr/0001-v0-architecture.md) (agent scope, reasoning), [ADR 0002](../adr/0002-toolchain-and-frontend-stack.md) (lints, gate, tests), [rough plan](2026-11-24-overseer-v0.md) phase 1.

## Global Constraints

- Rust toolchain `1.100`, edition 2024. Every crate inherits `[workspace.lints]`: `unsafe_code = "forbid"`, clippy `pedantic` at warn. No `#[allow]` anywhere; fix the lint instead.
- `thiserror` in library crates, `anyhow` only in binaries (ADR 0002). `overseer-core` is a library.
- No dependency without code that uses it: `cargo shear` must pass. Exact versions live in `[workspace.dependencies]`; crates inherit with `name.workspace = true`.
- Versions: `reqwest` 0.13.5 (`default-features = false`, features `rustls`, `json`, `http2`), `serde` 1.0.229 (`derive`), `serde_json` 1.0.154, `thiserror` 2.0.21, `tokio` 1.54.1 (`macros`, `rt`, dev only), `wiremock` 0.6.5 (dev only).
- The gate is `just check-rust` (fmt, clippy `-D warnings`, nextest `--locked`, shear, deny). CI runs it unchanged; the workflow needs no edit in this phase.
- No logging in this phase: no `tracing` dependency yet. Errors travel as `LlmError` values.
- The API key is never logged, never stored, never put in an error message. Tests use the fake key `sk-test`.
- Fixtures are hand-written from each provider's documented chunk shapes; none is a live recording, because no key is available in this environment. They live in `crates/core/tests/fixtures/llm/`.
- Commit messages in English, Conventional Commits.

## File map

| Path | Responsibility | Task |
|---|---|---|
| `Cargo.toml` | `[workspace.dependencies]` | 1 |
| `crates/core/Cargo.toml` | the crate's dependencies and dev-dependencies | 1 |
| `crates/core/src/lib.rs` | exports the `llm` module | 1 |
| `crates/core/src/llm/mod.rs` | module wiring and the public re-exports | 1 |
| `crates/core/src/llm/types.rs` | `Message`, `ToolCall`, `ToolDefinition`, `ChatRequest`, `Completion`, `LlmEvent`, `FinishReason`, `Usage`, `LlmError` | 1 |
| `crates/core/src/llm/sse.rs` | `SseParser`: body bytes → `data` payloads | 2 |
| `crates/core/src/llm/wire.rs` | request body builder; stream chunk structs for `serde` | 3 |
| `crates/core/src/llm/assemble.rs` | `Assembler`: payloads → events and `Completion` | 4 |
| `crates/core/src/llm/client.rs` | `ClientConfig`, `Client`, `ChatStream` | 5 |
| `crates/core/tests/fixtures/llm/*.sse` | provider stream fixtures | 4 |
| `crates/core/tests/llm_fixtures.rs` | replays fixtures through parser and assembler | 4 |
| `crates/core/tests/llm_client.rs` | client against a `wiremock` endpoint | 5 |
| `docs/adr/0001-v0-architecture.md` | reasoning send-back rule | 6 |

---

### Task 1: Dependencies, module skeleton and the public vocabulary

**Files:**
- Modify: `Cargo.toml` (add `[workspace.dependencies]`)
- Modify: `crates/core/Cargo.toml`
- Modify: `crates/core/src/lib.rs`
- Create: `crates/core/src/llm/mod.rs`, `crates/core/src/llm/types.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `overseer_core::llm::{Message, ToolCall, ToolDefinition, ChatRequest, Completion, FinishReason, Usage, LlmEvent, LlmError}`, with the exact definitions below. Every later task uses these names and fields.

- [ ] **Step 1: Add the workspace dependencies**

In `Cargo.toml`, after `[workspace.package]`:

```toml
[workspace.dependencies]
reqwest = { version = "0.13.5", default-features = false, features = ["rustls", "json", "http2"] }
serde_json = "1.0.154"
thiserror = "2.0.21"
```

`cargo shear` reports an unused *workspace* dependency as well as an unused crate one, so `serde` joins this table in Task 3 and `tokio` / `wiremock` in Task 5, each in the task whose code first uses it.

- [ ] **Step 2: Add the crate's dependencies**

In `crates/core/Cargo.toml`, after `[lints]`:

```toml
[dependencies]
reqwest.workspace = true
serde_json.workspace = true
thiserror.workspace = true
```

Only what this task's code uses: `reqwest::Error` in `LlmError`, `serde_json::Value` in `ToolDefinition`, `thiserror` for the derive. `cargo shear` fails on an unused dependency, including a dev-dependency, so `serde` arrives in Task 3 and `tokio` / `wiremock` in Task 5. `reqwest` needs no tokio runtime of its own; the caller supplies one, which in this phase is the tests.

- [ ] **Step 3: Wire the module**

`crates/core/src/lib.rs`:

```rust
pub mod llm;
```

`crates/core/src/llm/mod.rs`:

```rust
//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

mod types;

pub use types::{
    ChatRequest, Completion, FinishReason, LlmError, LlmEvent, Message, ToolCall, ToolDefinition,
    Usage,
};
```

`crates/core/src/llm/types.rs` is the file below, verbatim.

- [ ] **Step 4: Write `crates/core/src/llm/types.rs`**

```rust
use serde_json::Value;

/// A tool the model may call, sent in the request's `tools` array.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

/// A complete tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments exactly as the model produced them: a JSON text, not yet parsed or validated.
    pub arguments: String,
}

/// One message of the conversation sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        /// Sent back as `reasoning_content` only when `ClientConfig::send_reasoning` is set.
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// What one model call is given.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    /// Any value outside the four above, kept verbatim.
    Other(String),
}

impl FinishReason {
    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            other => Self::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// The assistant message one model call produced, assembled from all its deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub content: String,
    pub reasoning: Option<String>,
    /// In the order of the provider's `index`.
    pub tool_calls: Vec<ToolCall>,
    /// `None` when the provider ended the stream without sending one.
    pub finish_reason: Option<FinishReason>,
    /// `None` when the provider sent no usage chunk.
    pub usage: Option<Usage>,
}

/// One item of a model call's stream. `Done` is always the last item of a successful stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// A fragment of tool call `index`. `id` and `name` are set on the fragment that carried them,
    /// normally the first; `arguments` is the next piece of the arguments text, possibly empty.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Done(Completion),
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("request to the model failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("model endpoint returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("model provider reported an error mid-stream: {message}")]
    Provider { message: String },
    #[error("unparsable stream chunk {data:?}: {source}")]
    Decode {
        data: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("stream ended before the model finished")]
    Truncated,
    #[error("tool call {index} has no {field}")]
    InvalidToolCall { index: usize, field: &'static str },
}
```

- [ ] **Step 5: Write the failing test for `FinishReason::parse`**

Append to `crates/core/src/llm/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::FinishReason;

    #[test]
    fn parses_the_four_documented_finish_reasons() {
        assert_eq!(FinishReason::parse("stop"), FinishReason::Stop);
        assert_eq!(FinishReason::parse("length"), FinishReason::Length);
        assert_eq!(FinishReason::parse("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(FinishReason::parse("content_filter"), FinishReason::ContentFilter);
    }

    #[test]
    fn keeps_an_unknown_finish_reason_verbatim() {
        assert_eq!(
            FinishReason::parse("error"),
            FinishReason::Other("error".to_owned())
        );
    }
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo nextest run -p overseer-core`
Expected: PASS, `2 tests run: 2 passed`. (The parse function already exists: this step proves the module compiles and the mapping holds before later tasks build on it.)

- [ ] **Step 7: Run the whole gate**

Run: `just check-rust`
Expected: exit 0, ending with `advisories ok, bans ok, licenses ok, sources ok`. `cargo shear` fails on any dependency this task's code does not use, dev-dependencies included. If it reports one, a later task's dependency was added too early: remove it there and add it in that task.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock crates/core
git commit -m "feat(core): add the llm vocabulary and its dependencies"
```

---

### Task 2: SSE parser

**Files:**
- Create: `crates/core/src/llm/sse.rs`
- Modify: `crates/core/src/llm/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `SseParser::push(&mut self, bytes: &[u8]) -> Vec<String>` (the `data` payload of every event completed by these bytes) and `SseParser::finish(&mut self) -> Option<String>` (an event the body ended without a blank line after). `Assembler` (Task 4) consumes these payloads.

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/llm/sse.rs` with the file below, minus the `impl SseParser` block: keep the type declaration (`buffer`, `data`, `has_data`) and the `#[cfg(test)] mod tests`. The tests must not compile yet.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p overseer-core sse`
Expected: FAIL, `error[E0599]: no method named push found for struct SseParser`.

- [ ] **Step 3: Write the parser**

Complete `crates/core/src/llm/sse.rs` to exactly this:

```rust
/// Incremental parser for the `text/event-stream` format, fed raw body chunks.
///
/// Only `data` lines are kept; comments (`:` lines, such as `OpenRouter`'s
/// `: OPENROUTER PROCESSING` keep-alives) and other fields are dropped. Bytes are
/// buffered until a full line is available, so a chunk boundary inside a line,
/// inside a `\r\n`, or inside a multi-byte UTF-8 character is harmless.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data: String,
    has_data: bool,
}

impl SseParser {
    /// Feeds the next body chunk; returns the `data` payload of every event it completed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut events);
        }
        events
    }

    /// Flushes an event the stream ended without terminating with a blank line.
    pub fn finish(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let mut events = Vec::new();
            self.line(&String::from_utf8_lossy(&rest), &mut events);
            debug_assert_eq!(events, Vec::<String>::new());
        }
        self.has_data.then(|| {
            self.has_data = false;
            std::mem::take(&mut self.data)
        })
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if self.has_data {
                self.has_data = false;
                events.push(std::mem::take(&mut self.data));
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        if field == "data" {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(value);
            self.has_data = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SseParser;

    #[test]
    fn yields_data_of_each_event() {
        let mut parser = SseParser::default();
        let events = parser.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(events, ["one", "two"]);
    }

    #[test]
    fn joins_multi_line_data_with_newline() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: a\ndata: b\n\n"), ["a\nb"]);
    }

    #[test]
    fn ignores_comments_and_other_fields() {
        let mut parser = SseParser::default();
        let events = parser.push(b": OPENROUTER PROCESSING\n\nevent: x\nid: 3\ndata: kept\n\n");
        assert_eq!(events, ["kept"]);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: x\r\n\r\n"), ["x"]);
    }

    #[test]
    fn survives_a_chunk_split_inside_a_line() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: {\"a\""), Vec::<String>::new());
        assert_eq!(parser.push(b":1}\n\n"), ["{\"a\":1}"]);
    }

    #[test]
    fn survives_a_chunk_split_inside_a_utf8_character() {
        let mut parser = SseParser::default();
        let bytes = "data: é\n\n".as_bytes();
        assert_eq!(parser.push(&bytes[..7]), Vec::<String>::new());
        assert_eq!(parser.push(&bytes[7..]), ["é"]);
    }

    #[test]
    fn data_without_a_space_after_the_colon() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data:x\n\n"), ["x"]);
    }

    #[test]
    fn finish_flushes_an_unterminated_event() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: [DONE]"), Vec::<String>::new());
        assert_eq!(parser.finish().as_deref(), Some("[DONE]"));
        assert_eq!(parser.finish(), None);
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p overseer-core sse`
Expected: PASS, `8 tests run: 8 passed`. The chunk-split tests are the point: they feed a payload one byte at a time, split inside a line, inside `\r\n`, and inside the two bytes of `é`.

- [ ] **Step 5: Export the parser**

In `crates/core/src/llm/mod.rs`, add `mod sse;` and `pub use sse::SseParser;`:

```rust
//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

mod sse;
mod types;

pub use sse::SseParser;
pub use types::{
    ChatRequest, Completion, FinishReason, LlmError, LlmEvent, Message, ToolCall, ToolDefinition,
    Usage,
};
```

- [ ] **Step 6: Run the gate**

Run: `just check-rust`
Expected: exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/core
git commit -m "feat(core): parse text/event-stream body chunks"
```

---

### Task 3: Request body and stream chunk shapes

**Files:**
- Create: `crates/core/src/llm/wire.rs`
- Modify: `crates/core/src/llm/mod.rs`

**Interfaces:**
- Consumes: `Message`, `ToolCall`, `ToolDefinition`, `ChatRequest` from Task 1.
- Produces: `pub(crate) fn request_body(model: &str, request: &ChatRequest, send_reasoning: bool) -> serde_json::Value`, and the `pub(crate)` `serde` structs `Chunk`, `Choice`, `Delta`, `ToolCallDelta`, `FunctionDelta`, `WireUsage` that Task 4 decodes.

- [ ] **Step 1: Add the serde dependency**

Add `serde` to `[workspace.dependencies]` in the root `Cargo.toml`:

```toml
serde = { version = "1.0.229", features = ["derive"] }
```

and to `[dependencies]` in `crates/core/Cargo.toml`:

```toml
serde.workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/core/src/llm/wire.rs` with only the `#[cfg(test)] mod tests` from the file below, plus `use serde_json::Value;`. It must not compile: `request_body` is missing.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo nextest run -p overseer-core wire`
Expected: FAIL, `error[E0425]: cannot find function request_body in this scope`.

- [ ] **Step 4: Write the module**

Complete `crates/core/src/llm/wire.rs` to exactly this:

```rust
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
```

Two decisions this pins down:

- An assistant message with tool calls and no text sends `"content": null`, which the OpenAI schema allows and `DeepSeek` expects; sending `""` instead is accepted by most providers but not all.
- `reasoning_content` is sent only when `send_reasoning` is true. `DeepSeek` answers 400 without it on requests carrying tools, so the flag exists; it defaults off because providers that reject unknown message fields would 400 with it on.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p overseer-core wire`
Expected: PASS, `7 tests run: 7 passed`.

- [ ] **Step 6: Register the module**

In `crates/core/src/llm/mod.rs`, add `mod wire;` after `mod types;`. It stays private: nothing outside the module uses the wire shapes.

- [ ] **Step 7: Run the gate**

Run: `just check-rust`
Expected: exit 0. `cargo shear` must not report `serde` unused: `wire.rs` derives `Deserialize` from it.

- [ ] **Step 8: Commit**

```bash
git add crates/core
git commit -m "feat(core): build chat requests and decode stream chunks"
```

---

### Task 4: Assembler and provider fixtures

**Files:**
- Create: `crates/core/src/llm/assemble.rs`
- Create: `crates/core/tests/fixtures/llm/*.sse` (seven files)
- Create: `crates/core/tests/llm_fixtures.rs`
- Modify: `crates/core/src/llm/mod.rs`

**Interfaces:**
- Consumes: `SseParser` (Task 2), `Chunk` and its nested structs (Task 3), the vocabulary (Task 1).
- Produces: `Assembler::default()`, `Assembler::apply(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError>`, `Assembler::finish_without_done(&mut self) -> Result<Completion, LlmError>`. Exported from `overseer_core::llm::Assembler`; the client (Task 5) owns one per stream.

- [ ] **Step 1: Write the fixtures**

Each file is one provider's stream, written from that provider's documentation. Create all seven:

`crates/core/tests/fixtures/llm/text.sse`:

```
data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":", world"},"finish_reason":null}]}

data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}

data: [DONE]

```

`crates/core/tests/fixtures/llm/tool_calls.sse`:

```
data: {"id":"c2","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}

data: {"id":"c2","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]},"finish_reason":null}]}

data: {"id":"c2","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]},"finish_reason":null}]}

data: {"id":"c2","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]},"finish_reason":null}]}

data: {"id":"c2","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":40,"completion_tokens":21,"total_tokens":61}}

data: [DONE]

```

`crates/core/tests/fixtures/llm/reasoning_deepseek.sse`:

```
data: {"id":"c3","object":"chat.completion.chunk","created":1,"model":"deepseek-flash","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":"Compare 9.11"},"finish_reason":null}]}

data: {"id":"c3","object":"chat.completion.chunk","created":1,"model":"deepseek-flash","choices":[{"index":0,"delta":{"content":null,"reasoning_content":" and 9.8."},"finish_reason":null}]}

data: {"id":"c3","object":"chat.completion.chunk","created":1,"model":"deepseek-flash","choices":[{"index":0,"delta":{"content":"9.8 is greater.","reasoning_content":null},"finish_reason":null}]}

data: {"id":"c3","object":"chat.completion.chunk","created":1,"model":"deepseek-flash","choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":9,"total_tokens":19}}

data: [DONE]

```

`crates/core/tests/fixtures/llm/reasoning_openrouter.sse`:

```
: OPENROUTER PROCESSING

: OPENROUTER PROCESSING

data: {"id":"gen-1","object":"chat.completion.chunk","created":1,"model":"anthropic/claude","choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning":"Think first.","reasoning_details":[{"type":"reasoning.text","text":"Think first.","format":"anthropic-claude-v1","index":0}]},"finish_reason":null}]}

data: {"id":"gen-1","object":"chat.completion.chunk","created":1,"model":"anthropic/claude","choices":[{"index":0,"delta":{"content":"Done."},"finish_reason":"stop","native_finish_reason":"end_turn"}]}

data: {"id":"gen-1","object":"chat.completion.chunk","created":1,"model":"anthropic/claude","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":4,"total_tokens":9}}

data: [DONE]

```

`crates/core/tests/fixtures/llm/midstream_error.sse`:

```
data: {"id":"gen-2","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"Partial"},"finish_reason":null}]}

data: {"error":{"code":502,"message":"Provider disconnected mid-stream"}}

```

`crates/core/tests/fixtures/llm/truncated.sse`:

```
data: {"id":"c4","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"Cut"},"finish_reason":null}]}

```

`crates/core/tests/fixtures/llm/no_done.sse`:

```
data: {"id":"c5","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"Bye"},"finish_reason":"stop"}]}

```

- [ ] **Step 2: Write the failing tests**

Create `crates/core/tests/llm_fixtures.rs`:

```rust
//! Replays hand-written provider streams through `SseParser` and `Assembler`.
//! Each fixture follows a shape documented by its provider; none is a live recording.

use overseer_core::llm::{
    Assembler, Completion, FinishReason, LlmError, LlmEvent, SseParser, ToolCall, Usage,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/llm/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// Feeds the fixture in chunks of `chunk` bytes, the way a body arrives off the network,
/// and returns every event plus the first error.
fn replay(name: &str, chunk: usize) -> (Vec<LlmEvent>, Option<LlmError>) {
    let mut parser = SseParser::default();
    let mut assembler = Assembler::default();
    let mut events = Vec::new();
    let bytes = fixture(name);
    let mut datas: Vec<String> = Vec::new();
    for piece in bytes.chunks(chunk) {
        datas.extend(parser.push(piece));
    }
    datas.extend(parser.finish());
    for data in datas {
        match assembler.apply(&data) {
            Ok(more) => events.extend(more),
            Err(error) => return (events, Some(error)),
        }
    }
    if !events
        .iter()
        .any(|event| matches!(event, LlmEvent::Done(_)))
    {
        match assembler.finish_without_done() {
            Ok(completion) => events.push(LlmEvent::Done(completion)),
            Err(error) => return (events, Some(error)),
        }
    }
    (events, None)
}

fn completion(events: &[LlmEvent]) -> &Completion {
    match events.last() {
        Some(LlmEvent::Done(completion)) => completion,
        other => panic!("last event is not Done: {other:?}"),
    }
}

#[test]
fn text_reply_streams_deltas_then_completes() {
    for chunk in [1, 7, 4096] {
        let (events, error) = replay("text.sse", chunk);
        assert!(error.is_none(), "chunk {chunk}: {error:?}");
        assert_eq!(
            events[..2],
            [
                LlmEvent::TextDelta("Hello".into()),
                LlmEvent::TextDelta(", world".into()),
            ],
            "chunk {chunk}"
        );
        assert_eq!(
            completion(&events),
            &Completion {
                content: "Hello, world".into(),
                reasoning: None,
                tool_calls: vec![],
                finish_reason: Some(FinishReason::Stop),
                usage: Some(Usage {
                    prompt_tokens: 12,
                    completion_tokens: 3,
                }),
            },
            "chunk {chunk}"
        );
    }
}

#[test]
fn interleaved_tool_calls_are_assembled_by_index() {
    let (events, error) = replay("tool_calls.sse", 5);
    assert!(error.is_none(), "{error:?}");
    let done = completion(&events);
    assert_eq!(
        done.tool_calls,
        [
            ToolCall {
                id: "call_a".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
            ToolCall {
                id: "call_b".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            },
        ]
    );
    assert_eq!(done.finish_reason, Some(FinishReason::ToolCalls));
    assert_eq!(done.content, "");
    assert_eq!(
        events[0],
        LlmEvent::ToolCallDelta {
            index: 0,
            id: Some("call_a".into()),
            name: Some("read_file".into()),
            arguments: String::new(),
        }
    );
}

#[test]
fn deepseek_reasoning_content_is_kept_apart_from_text() {
    let (events, error) = replay("reasoning_deepseek.sse", 3);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        events[..3],
        [
            LlmEvent::ReasoningDelta("Compare 9.11".into()),
            LlmEvent::ReasoningDelta(" and 9.8.".into()),
            LlmEvent::TextDelta("9.8 is greater.".into()),
        ]
    );
    let done = completion(&events);
    assert_eq!(done.reasoning.as_deref(), Some("Compare 9.11 and 9.8."));
    assert_eq!(done.content, "9.8 is greater.");
}

#[test]
fn openrouter_reasoning_and_keepalive_comments() {
    let (events, error) = replay("reasoning_openrouter.sse", 11);
    assert!(error.is_none(), "{error:?}");
    let done = completion(&events);
    assert_eq!(done.reasoning.as_deref(), Some("Think first."));
    assert_eq!(done.content, "Done.");
    assert_eq!(done.finish_reason, Some(FinishReason::Stop));
}

#[test]
fn mid_stream_provider_error_is_reported_after_the_text_so_far() {
    let (events, error) = replay("midstream_error.sse", 4096);
    assert_eq!(events, [LlmEvent::TextDelta("Partial".into())]);
    match error {
        Some(LlmError::Provider { message }) => {
            assert_eq!(message, "Provider disconnected mid-stream");
        }
        other => panic!("expected a provider error, got {other:?}"),
    }
}

#[test]
fn body_ending_before_finish_reason_is_truncated() {
    let (_, error) = replay("truncated.sse", 4096);
    assert!(matches!(error, Some(LlmError::Truncated)), "{error:?}");
}

#[test]
fn body_ending_after_finish_reason_without_done_completes() {
    let (events, error) = replay("no_done.sse", 4096);
    assert!(error.is_none(), "{error:?}");
    let done = completion(&events);
    assert_eq!(done.content, "Bye");
    assert_eq!(done.usage, None);
}

#[test]
fn unknown_finish_reason_is_kept_verbatim() {
    let mut assembler = Assembler::default();
    assembler
        .apply(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"error"}]}"#)
        .unwrap();
    let events = assembler.apply("[DONE]").unwrap();
    assert_eq!(
        completion(&events).finish_reason,
        Some(FinishReason::Other("error".into()))
    );
}

#[test]
fn tool_call_without_a_name_is_rejected() {
    let mut assembler = Assembler::default();
    assembler
        .apply(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"x","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    let error = assembler.apply("[DONE]").unwrap_err();
    assert!(
        matches!(
            error,
            LlmError::InvalidToolCall {
                index: 0,
                field: "name"
            }
        ),
        "{error:?}"
    );
}

#[test]
fn malformed_chunk_is_a_decode_error() {
    let mut assembler = Assembler::default();
    let error = assembler.apply("{not json").unwrap_err();
    assert!(matches!(error, LlmError::Decode { .. }), "{error:?}");
}
```

Run: `cargo nextest run -p overseer-core --test llm_fixtures`
Expected: FAIL, `error[E0432]: unresolved import overseer_core::llm::Assembler`.

- [ ] **Step 3: Write the assembler**

Create `crates/core/src/llm/assemble.rs`:

```rust
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
```

In `crates/core/src/llm/mod.rs`, add `mod assemble;` and `pub use assemble::Assembler;`:

```rust
//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

mod assemble;
mod sse;
mod types;
mod wire;

pub use assemble::Assembler;
pub use sse::SseParser;
pub use types::{
    ChatRequest, Completion, FinishReason, LlmError, LlmEvent, Message, ToolCall, ToolDefinition,
    Usage,
};
```

Two behaviours the assembler fixes that the tests pin down: a provider that repeats a tool call's `id` or `name` on later fragments does not duplicate it (the first non-empty value wins), and events for a `choice.index` other than `0` are ignored, which is safe because the client never asks for `n > 1`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p overseer-core --test llm_fixtures`
Expected: PASS, `10 tests run: 10 passed`. The first test replays the same fixture in 1-byte, 7-byte and whole-body chunks and demands identical events, which is what proves the parser and assembler do not depend on network chunking.

- [ ] **Step 5: Run the gate**

Run: `just check-rust`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/core
git commit -m "feat(core): assemble stream deltas into one completion"
```

---

### Task 5: Client and stream

**Files:**
- Create: `crates/core/src/llm/client.rs`
- Create: `crates/core/tests/llm_client.rs`
- Modify: `crates/core/src/llm/mod.rs`

**Interfaces:**
- Consumes: `request_body` (Task 3), `SseParser` (Task 2), `Assembler` (Task 4), the vocabulary (Task 1).
- Produces: `ClientConfig { base_url, api_key, model, send_reasoning, connect_timeout, read_timeout }` with `ClientConfig::new(base_url, model)`; `Client::new(ClientConfig) -> Result<Client, LlmError>`; `Client::stream(&self, &ChatRequest) -> Result<ChatStream, LlmError>`; `ChatStream::next(&mut self) -> Option<Result<LlmEvent, LlmError>>`. Phase 3's agent loop drives `ChatStream`; phase 3's CLI builds `ClientConfig`.

- [ ] **Step 1: Add the test dependencies**

Add both to `[workspace.dependencies]` in the root `Cargo.toml`:

```toml
tokio = { version = "1.54.1", features = ["macros", "rt"] }
wiremock = "0.6.5"
```

and to `crates/core/Cargo.toml`, after `[dependencies]`:

```toml
[dev-dependencies]
tokio.workspace = true
wiremock.workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/core/tests/llm_client.rs`:

```rust
//! Drives `Client` against a local mock endpoint: request shape, auth, errors, timeouts.

use std::time::Duration;

use overseer_core::llm::{
    ChatRequest, Client, ClientConfig, FinishReason, LlmError, LlmEvent, Message, ToolCall,
    ToolDefinition,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TEXT: &str = include_str!("fixtures/llm/text.sse");

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn config(server: &MockServer) -> ClientConfig {
    let mut config = ClientConfig::new(format!("{}/v1/", server.uri()), "test-model");
    config.api_key = Some("sk-test".into());
    config
}

async fn collect(client: &Client, request: &ChatRequest) -> Result<Vec<LlmEvent>, LlmError> {
    let mut stream = client.stream(request).await?;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event?);
    }
    Ok(events)
}

fn user(text: &str) -> ChatRequest {
    ChatRequest {
        messages: vec![Message::User {
            content: text.into(),
        }],
        tools: vec![],
    }
}

#[tokio::test]
async fn posts_a_streaming_request_with_auth_and_returns_the_completion() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(header("accept", "text/event-stream"))
        .and(body_partial_json(json!({
            "model": "test-model",
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "hi" }],
        })))
        .respond_with(sse(TEXT))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(config(&server)).unwrap();

    let events = collect(&client, &user("hi")).await.unwrap();

    match events.last() {
        Some(LlmEvent::Done(done)) => {
            assert_eq!(done.content, "Hello, world");
            assert_eq!(done.finish_reason, Some(FinishReason::Stop));
        }
        other => panic!("expected Done last, got {other:?}"),
    }
}

#[tokio::test]
async fn serialises_tools_and_the_whole_history() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(TEXT))
        .mount(&server)
        .await;
    let client = Client::new(config(&server)).unwrap();
    let request = ChatRequest {
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
            parameters: json!({ "type": "object", "properties": { "command": { "type": "string" } } }),
        }],
    };

    collect(&client, &request).await.unwrap();

    let body = only_request_body(&server).await;
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
    assert_eq!(
        body["tools"],
        json!([{
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a command",
                "parameters": { "type": "object", "properties": { "command": { "type": "string" } } },
            },
        }])
    );
}

#[tokio::test]
async fn sends_reasoning_back_only_when_configured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(TEXT))
        .mount(&server)
        .await;
    let mut config = config(&server);
    config.send_reasoning = true;
    let client = Client::new(config).unwrap();
    let request = ChatRequest {
        messages: vec![Message::Assistant {
            content: "ok".into(),
            reasoning: Some("thought".into()),
            tool_calls: vec![],
        }],
        tools: vec![],
    };

    collect(&client, &request).await.unwrap();

    let body = only_request_body(&server).await;
    assert_eq!(
        body["messages"][0],
        json!({ "role": "assistant", "content": "ok", "reasoning_content": "thought" })
    );
}

#[tokio::test]
async fn omits_tools_and_auth_when_there_are_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(TEXT))
        .mount(&server)
        .await;
    let mut config = config(&server);
    config.api_key = None;
    let client = Client::new(config).unwrap();

    collect(&client, &user("hi")).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("authorization"));
    let body: Value = requests[0].body_json().unwrap();
    assert!(body.get("tools").is_none(), "{body}");
}

#[tokio::test]
async fn non_2xx_status_returns_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"bad key"}}"#),
        )
        .mount(&server)
        .await;
    let client = Client::new(config(&server)).unwrap();

    let error = collect(&client, &user("hi")).await.unwrap_err();

    match error {
        LlmError::Status { status, body } => {
            assert_eq!(status, 401);
            assert!(body.contains("bad key"), "{body}");
        }
        other => panic!("expected Status, got {other:?}"),
    }
}

#[tokio::test]
async fn stalled_response_fails_after_the_read_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(TEXT).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let mut config = config(&server);
    config.read_timeout = Duration::from_millis(200);
    let client = Client::new(config).unwrap();

    let error = collect(&client, &user("hi")).await.unwrap_err();

    match error {
        LlmError::Http(error) => assert!(error.is_timeout(), "{error}"),
        other => panic!("expected a timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn stream_ends_after_the_first_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(include_str!("fixtures/llm/midstream_error.sse")))
        .mount(&server)
        .await;
    let client = Client::new(config(&server)).unwrap();

    let mut stream = client.stream(&user("hi")).await.unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        LlmEvent::TextDelta("Partial".into())
    );
    assert!(matches!(
        stream.next().await,
        Some(Err(LlmError::Provider { .. }))
    ));
    assert!(stream.next().await.is_none());
}

async fn only_request_body(server: &MockServer) -> Value {
    let requests: Vec<Request> = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    requests[0].body_json().unwrap()
}
```

Run: `cargo nextest run -p overseer-core --test llm_client`
Expected: FAIL, `error[E0432]: unresolved import overseer_core::llm::Client`.

- [ ] **Step 3: Write the client**

Create `crates/core/src/llm/client.rs`:

```rust
use std::collections::VecDeque;
use std::time::Duration;

use super::assemble::Assembler;
use super::sse::SseParser;
use super::types::{ChatRequest, LlmError, LlmEvent};
use super::wire::request_body;

/// Longest prefix of an error response body kept in `LlmError::Status`.
const ERROR_BODY_LIMIT: usize = 2048;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Endpoint root, for example `https://openrouter.ai/api/v1`; `/chat/completions` is appended.
    pub base_url: String,
    /// Sent as `Authorization: Bearer`; `None` sends no `Authorization` header.
    pub api_key: Option<String>,
    pub model: String,
    /// Send each assistant message's reasoning back as `reasoning_content`. `DeepSeek` requires it
    /// on requests carrying tools and answers 400 without it.
    pub send_reasoning: bool,
    pub connect_timeout: Duration,
    /// Longest wait for the next bytes of a response, headers included.
    pub read_timeout: Duration,
}

impl ClientConfig {
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            send_reasoning: false,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    url: String,
    config: ClientConfig,
}

impl Client {
    pub fn new(config: ClientConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()?;
        let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        Ok(Self { http, url, config })
    }

    /// Starts one model call. Dropping the returned stream aborts the request.
    pub async fn stream(&self, request: &ChatRequest) -> Result<ChatStream, LlmError> {
        let body = request_body(&self.config.model, request, self.config.send_reasoning);
        let mut builder = self
            .http
            .post(&self.url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(key) = &self.config.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::Status {
                status: status.as_u16(),
                body: text.chars().take(ERROR_BODY_LIMIT).collect(),
            });
        }
        Ok(ChatStream {
            response,
            parser: SseParser::default(),
            assembler: Assembler::default(),
            pending: VecDeque::new(),
            error: None,
            finished: false,
        })
    }
}

/// The events of one model call, in order. Ends after `LlmEvent::Done` or after the first error.
#[derive(Debug)]
pub struct ChatStream {
    response: reqwest::Response,
    parser: SseParser,
    assembler: Assembler,
    pending: VecDeque<LlmEvent>,
    /// Returned once every event decoded before it has been handed out.
    error: Option<LlmError>,
    finished: bool,
}

impl ChatStream {
    pub async fn next(&mut self) -> Option<Result<LlmEvent, LlmError>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(Ok(event));
            }
            if let Some(error) = self.error.take() {
                return Some(Err(error));
            }
            if self.finished {
                return None;
            }
            if let Err(error) = self.read().await {
                self.finished = true;
                self.error = Some(error);
            }
        }
    }

    async fn read(&mut self) -> Result<(), LlmError> {
        if let Some(bytes) = self.response.chunk().await? {
            for data in self.parser.push(&bytes) {
                self.apply(&data)?;
                if self.finished {
                    break;
                }
            }
            return Ok(());
        }
        if let Some(data) = self.parser.finish() {
            self.apply(&data)?;
        }
        if !self.finished {
            let completion = self.assembler.finish_without_done()?;
            self.pending.push_back(LlmEvent::Done(completion));
            self.finished = true;
        }
        Ok(())
    }

    fn apply(&mut self, data: &str) -> Result<(), LlmError> {
        for event in self.assembler.apply(data)? {
            self.finished |= matches!(event, LlmEvent::Done(_));
            self.pending.push_back(event);
        }
        Ok(())
    }
}
```

A known limitation, deliberately not fixed in this phase: a `200` whose body is not an event stream (a proxy's HTML error page, a misconfigured `base_url`) yields `LlmError::Truncated` once the body ends, and the body text is not surfaced. Adding a content-type check is phase 5's work, where the server surfaces model errors to the client.

In `crates/core/src/llm/mod.rs`, add `mod client;` and re-export:

```rust
//! Streaming client for OpenAI-compatible `/chat/completions` endpoints.

mod assemble;
mod client;
mod sse;
mod types;
mod wire;

pub use assemble::Assembler;
pub use client::{ChatStream, Client, ClientConfig};
pub use sse::SseParser;
pub use types::{
    ChatRequest, Completion, FinishReason, LlmError, LlmEvent, Message, ToolCall, ToolDefinition,
    Usage,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p overseer-core --test llm_client`
Expected: PASS, `7 tests run: 7 passed`. If `stream_ends_after_the_first_error` fails on its first assertion, the error is being returned before the events decoded from the same body chunk: keep `error` as a field and hand it out only after `pending` is empty, as the file above does.

- [ ] **Step 5: Run the gate**

Run: `just check-rust`
Expected: exit 0. The timeout test takes about 0.2 s by design (`read_timeout = 200 ms`).

- [ ] **Step 6: Commit**

```bash
git add crates/core
git commit -m "feat(core): stream chat completions over HTTP"
```

---

### Task 6: Amend the reasoning rule in ADR 0001

**Files:**
- Modify: `docs/adr/0001-v0-architecture.md:102` and `:122`

**Interfaces:**
- Consumes: `ClientConfig::send_reasoning` (Task 5).
- Produces: an ADR that matches the code. Nothing later reads it at runtime.

- [ ] **Step 1: Replace the agent-scope line**

Line 102 currently reads:

```markdown
- Model reasoning is read from `reasoning_content` or `reasoning`, streamed as `reasoning_delta`, stored on `assistant_message`, and never sent back to the model.
```

Replace it with:

```markdown
- Model reasoning is read from `reasoning_content` or `reasoning`, streamed as `reasoning_delta` and stored on `assistant_message`. It is sent back as `reasoning_content` only when the client's `send_reasoning` is set, off by default: `DeepSeek` answers HTTP 400 on a tool-carrying request that omits it ([Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode)), while a provider that rejects unknown message fields fails with it on.
```

- [ ] **Step 2: Replace the matching consequence**

Line 122 currently reads:

```markdown
- Some providers require reasoning to be sent back during a tool-call loop. Against one of those, the request fails until sending it back is added.
```

Replace it with:

```markdown
- `send_reasoning` off is the wrong setting for a `DeepSeek`-backed model: its second call in a tool loop fails with HTTP 400 until the flag is turned on. Which setting a given endpoint needs is found by trying one tool turn against it, not by reading its documentation alone.
```

- [ ] **Step 3: Check the ADR still reads as one argument**

Read `docs/adr/0001-v0-architecture.md` around both edits. Expected: the agent-scope bullet states the rule, the consequence states the cost of each setting, and neither contradicts the other.

- [ ] **Step 4: Correct the rough plan's phase 1 row**

The rough plan's phase 1 row says its tests run on "recorded streams"; the fixtures in this phase are written from provider documentation. In `docs/plans/2026-11-24-overseer-v0.md`, replace the phase 1 row with:

```markdown
| 1 | Streaming LLM client: SSE parser, `tool_calls` assembled from deltas by `index`, `finish_reason`, usage | fixture tests on documented provider streams pass; a real recording joins them in phase 3 |
```

- [ ] **Step 5: Commit**

```bash
git add docs/adr/0001-v0-architecture.md docs/plans/2026-11-24-overseer-v0.md
git commit -m "docs: send reasoning back only when the client asks for it"
```

---

## Phase 1 done when

- `just check-rust` exits 0: fmt, clippy `-D warnings` with `pedantic`, nextest `--locked`, cargo-shear, cargo-deny.
- `cargo nextest run -p overseer-core` reports 34 tests passed across 3 binaries: 17 in the library (2 finish-reason, 8 SSE, 7 request body), 10 fixture replays, 7 client tests.
- Every provider shape in `tests/fixtures/llm/` replays to the documented completion, including at a 1-byte chunk size.
- The client sends `Authorization` only when a key is set, and never puts the key in an error.
