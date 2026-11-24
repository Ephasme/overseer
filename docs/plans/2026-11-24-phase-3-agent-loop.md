# Phase 3: agent loop and CLI — implementation plan

> **For the implementing agent:** Execute one task at a time. Tick each checkbox
> (`- [ ]` → `- [x]`) as you finish a step, and stop to ask if any step is unclear —
> never guess.

**Goal:** one turn of the agent — user text in, model calls and tool calls until the model stops — expressed entirely as events, plus a headless CLI that runs it against a configured model.

**Architecture:** `crates/core/src/agent` holds the loop. `events` defines what a turn emits, split into durable and ephemeral events exactly as ADR 0001 lists them. `conversation` folds the durable events back into the `Vec<Message>` the model is sent, so the event stream is the single source of truth (phase 5 will feed it stored events instead of live ones). `Agent::run_turn` drives `Client::stream` (phase 1) and `ToolRegistry::call` (phase 2), passing the same `CancellationToken` to both. `crates/cli` loads a TOML config and runs turns from stdin, printing events.

**Tech Stack:** Rust 1.100 (edition 2024), tokio, tokio-util (`CancellationToken`), the phase 1 LLM client and phase 2 tool registry, wiremock (dev, already present), toml 1.2 and anyhow 1.0 in the CLI.

**Spec:** [ADR 0001](../adr/0001-v0-architecture.md) (event log, tool-call results, cancellation), [rough plan](2026-11-24-overseer-v0.md) phase 3.

## Global Constraints

- Rust toolchain `1.100`, edition 2024, `unsafe_code = "forbid"`, clippy `pedantic` at warn with `-D warnings`. No `#[allow]`.
- `thiserror` in `overseer-core`, `anyhow` in `overseer-cli` (ADR 0002).
- No dependency without code that uses it, including `[workspace.dependencies]`: `cargo shear` fails on all three kinds.
- The gate is `just check-rust`. CI runs it unchanged.
- Durable events are exactly ADR 0001's: `UserMessage`, `AssistantMessage`, `ToolResult`, `TurnEnd`, `Error`. Ephemeral: `TextDelta`, `ReasoningDelta`, `ToolCallDelta`, `ToolProgress`. `AgentEvent::is_durable` is the one place that decides.
- Every tool call in an `AssistantMessage` gets a `ToolResult`, in the same order. A cancelled turn still writes results for calls it did not run.
- A turn makes at most `AgentConfig::max_steps` model calls (default 25); the 26th never happens, and the turn ends with `TurnEndReason::MaxIterations`.
- Cancellation goes through one `CancellationToken`, shared by the model stream and the running tool. Dropping the `run_turn` future is not a cancellation path.
- Reasoning is carried on `AssistantMessage` and sent back only if `ClientConfig::send_reasoning` is set (phase 1, ADR 0001).
- No compaction: a request that the endpoint rejects for length ends the turn with `Error`.
- Commit messages in English, Conventional Commits.

## File map

| Path | Responsibility | Task |
|---|---|---|
| `crates/core/src/agent/mod.rs` | `Agent`, `AgentConfig`, `default_system_prompt` | 1, 2, 3 |
| `crates/core/src/agent/events.rs` | `AgentEvent`, `TurnEndReason`, `EventSink` | 1 |
| `crates/core/src/agent/conversation.rs` | durable events → model messages | 1 |
| `crates/core/src/lib.rs` | exports `agent` | 1 |
| `crates/core/tests/agent.rs` | the loop against a scripted model | 2, 3 |
| `crates/cli/src/config.rs` | TOML config and the API key from the environment | 4 |
| `crates/cli/src/main.rs` | stdin REPL over `Agent::run_turn` | 4 |
| `crates/cli/tests/repl.rs` | the binary against a scripted model | 4 |
| `docs/adr/0001-v0-architecture.md` | turn-end reasons, the step cap | 5 |
| `docs/plans/2026-11-24-overseer-v0.md` | phase 3 row | 5 |

---

### Task 1: Events and conversation

**Files:**
- Create: `crates/core/src/agent/mod.rs`, `crates/core/src/agent/events.rs`, `crates/core/src/agent/conversation.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `crate::llm::{Message, ToolCall}` (phase 1).
- Produces: `AgentEvent` (with `is_durable`), `TurnEndReason`, `EventSink` (blanket impl for `Vec<AgentEvent>`), `Conversation::new`, `Conversation::from_events`, `Conversation::messages`, `Conversation::push`. Tasks 2–4 use these names and fields.

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/agent/conversation.rs` with only the `#[cfg(test)] mod tests` block from Step 3, and `crates/core/src/agent/events.rs` with only the type definitions. Run `cargo nextest run -p overseer-core conversation` and expect `error[E0433]: failed to resolve: use of undeclared crate or module`.

- [ ] **Step 2: Wire the module**

`crates/core/src/lib.rs`:

```rust
pub mod agent;
pub mod llm;
pub mod tools;
```

`crates/core/src/agent/mod.rs`:

```rust
//! One turn of the agent: model calls and tool calls until the model stops, as events.

mod conversation;
mod events;

pub use conversation::Conversation;
pub use events::{AgentEvent, EventSink, TurnEndReason};
```

- [ ] **Step 3: Write the events**

`crates/core/src/agent/events.rs`:

```rust
use crate::llm::ToolCall;

/// Why a turn stopped. The turn always ends with exactly one `TurnEnd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndReason {
    /// The model answered without asking for a tool.
    Done,
    /// The caller cancelled; a running command was killed.
    Cancelled,
    /// `AgentConfig::max_steps` model calls were made and the model still asked for tools.
    MaxIterations,
    /// The model endpoint failed, or a request could not be built.
    Error,
}

/// What a turn emits, in order. The durable ones are ADR 0001's event log; the others exist only
/// for the live stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
        /// True when the turn was cancelled mid-reply; `text` is then what had arrived.
        interrupted: bool,
    },
    ToolResult {
        call_id: String,
        ok: bool,
        output: String,
    },
    TurnEnd {
        reason: TurnEndReason,
    },
    Error {
        message: String,
    },
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    ToolProgress {
        call_id: String,
        line: String,
    },
}

impl AgentEvent {
    /// Durable events are the ones phase 5 stores and replays; everything else is live only.
    #[must_use]
    pub fn is_durable(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. }
                | Self::AssistantMessage { .. }
                | Self::ToolResult { .. }
                | Self::TurnEnd { .. }
                | Self::Error { .. }
        )
    }
}

/// Where a turn's events go. Phase 5's session actor implements it over its broadcast channel.
pub trait EventSink: Send {
    fn emit(&mut self, event: AgentEvent);
}

impl EventSink for Vec<AgentEvent> {
    fn emit(&mut self, event: AgentEvent) {
        self.push(event);
    }
}
```

- [ ] **Step 4: Write the conversation**

`crates/core/src/agent/conversation.rs`:

```rust
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
    use crate::agent::AgentEvent;

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }
    }

    #[test]
    fn a_request_starts_with_the_system_prompt() {
        let conversation = Conversation::new("be brief");
        assert_eq!(
            conversation.messages(),
            [Message::System {
                content: "be brief".into()
            }]
        );
    }

    #[test]
    fn durable_events_become_messages_in_order() {
        let mut conversation = Conversation::new("sys");
        conversation.push(AgentEvent::UserMessage {
            text: "hi".into(),
        });
        conversation.push(AgentEvent::AssistantMessage {
            text: String::new(),
            reasoning: Some("think".into()),
            tool_calls: vec![tool_call()],
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
                    content: "a.rs".into(),
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
            reason: crate::agent::TurnEndReason::Done,
        });
        conversation.push(AgentEvent::Error {
            message: "boom".into(),
        });
        assert_eq!(conversation.messages().len(), 1);
    }

    #[test]
    fn rebuilding_from_events_matches_a_live_conversation() {
        let events = vec![
            AgentEvent::UserMessage {
                text: "hi".into(),
            },
            AgentEvent::AssistantMessage {
                text: "hello".into(),
                reasoning: None,
                tool_calls: vec![],
                interrupted: false,
            },
            AgentEvent::TurnEnd {
                reason: crate::agent::TurnEndReason::Done,
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

    #[test]
    fn durability_is_exactly_the_event_log() {
        assert!(AgentEvent::UserMessage { text: "x".into() }.is_durable());
        assert!(AgentEvent::TurnEnd {
            reason: crate::agent::TurnEndReason::Done
        }
        .is_durable());
        assert!(!AgentEvent::TextDelta("x".into()).is_durable());
        assert!(!AgentEvent::ToolProgress {
            call_id: "c".into(),
            line: "l".into()
        }
        .is_durable());
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo nextest run -p overseer-core conversation`
Expected: PASS, `5 tests run: 5 passed`.

- [ ] **Step 6: Run the gate**

Run: `just check-rust`
Expected: exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/core
git commit -m "feat(agent): define the turn events and rebuild the conversation from them"
```

---

### Task 2: The turn loop

**Files:**
- Modify: `crates/core/src/agent/mod.rs`
- Create: `crates/core/tests/agent.rs`

**Interfaces:**
- Consumes: `Conversation`, `AgentEvent`, `EventSink`, `TurnEndReason` (Task 1); `Client`, `ClientConfig`, `ChatRequest`, `LlmEvent`, `Completion` (phase 1); `ToolRegistry`, `ToolOutcome` (phase 2); `CancellationToken`.
- Produces: `Agent::new(client, tools, config)`, `Agent::run_turn(&mut self, input: &str, sink: &mut dyn EventSink, cancel: &CancellationToken) -> TurnEndReason`, `AgentConfig::new(system_prompt)` with `max_steps` (default 25), `default_system_prompt(cwd)`, `Agent::conversation()`. Task 4 builds the CLI on these.

- [ ] **Step 1: Add the tokio features the tests need**

In `Cargo.toml`, the workspace `tokio` line gains `rt-multi-thread`, for the multi-threaded CLI test in Task 4:

```toml
tokio = { version = "1.54.1", features = ["macros", "rt", "rt-multi-thread", "process", "io-util", "time", "fs"] }
```

- [ ] **Step 2: Write the loop**

`crates/core/src/agent/mod.rs` becomes:

```rust
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
    Cancelled { text: String, reasoning: String },
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
                            reasoning,
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
```

- [ ] **Step 3: Write the loop tests**

Create `crates/core/tests/agent.rs`:

```rust
//! Drives `Agent::run_turn` against a scripted OpenAI-compatible endpoint.

use std::collections::VecDeque;
use std::sync::Mutex;

use overseer_core::agent::{Agent, AgentConfig, AgentEvent, TurnEndReason, default_system_prompt};
use overseer_core::llm::{Client, ClientConfig};
use overseer_core::tools::ToolRegistry;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TEXT: &str = include_str!("fixtures/llm/text.sse");
const TOOL_CALLS: &str = include_str!("fixtures/llm/tool_calls.sse");
const SLOW_TOOL: &str = include_str!("fixtures/llm/slow_tool.sse");

/// Answers each request with the next scripted body, in order.
struct Script {
    responses: Mutex<VecDeque<ResponseTemplate>>,
}

impl Script {
    fn new(bodies: &[&str]) -> Self {
        Self {
            responses: Mutex::new(bodies.iter().map(|body| sse(body)).collect()),
        }
    }
}

impl Respond for Script {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.responses
            .lock()
            .expect("script lock")
            .pop_front()
            .expect("a scripted response per request")
    }
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn client(server: &MockServer) -> Client {
    Client::new(ClientConfig::new(format!("{}/v1", server.uri()), "test-model"))
        .expect("client builds")
}

fn agent(server: &MockServer, dir: &std::path::Path, max_steps: usize) -> Agent {
    let mut config = AgentConfig::new(default_system_prompt(dir));
    config.max_steps = max_steps;
    Agent::new(client(server), ToolRegistry::new(dir), config)
}

async fn mount_script(server: &MockServer, bodies: &[&str]) {
    Mock::given(method("POST"))
        .respond_with(Script::new(bodies))
        .mount(server)
        .await;
}

async fn run(agent: &mut Agent, text: &str) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let reason = agent
        .run_turn(text, &mut events, &CancellationToken::new())
        .await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnd { reason }),
        "the turn ends with exactly one TurnEnd"
    );
    events
}

fn assistant_messages(events: &[AgentEvent]) -> Vec<(&str, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AssistantMessage {
                text, interrupted, ..
            } => Some((text.as_str(), *interrupted)),
            _ => None,
        })
        .collect()
}

fn tool_results(events: &[AgentEvent]) -> Vec<(&str, bool, &str)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult { call_id, ok, output } => {
                Some((call_id.as_str(), *ok, output.as_str()))
            }
            _ => None,
        })
        .collect()
}

async fn request_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("requests recorded")
        .iter()
        .map(|request| request.body_json().expect("json body"))
        .collect()
}

#[tokio::test]
async fn a_plain_answer_ends_the_turn_after_one_model_call() {
    let server = MockServer::start().await;
    mount_script(&server, &[TEXT]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&server, dir.path(), 25);

    let events = run(&mut agent, "hi").await;

    assert_eq!(events.first(), Some(&AgentEvent::UserMessage { text: "hi".into() }));
    assert!(events.contains(&AgentEvent::TextDelta("Hello".into())));
    assert_eq!(assistant_messages(&events), [("Hello, world", false)]);
    assert!(tool_results(&events).is_empty());
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnd {
            reason: TurnEndReason::Done
        })
    );
    let bodies = request_bodies(&server).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert_eq!(bodies[0]["messages"][1]["content"], "hi");
    assert_eq!(bodies[0]["tools"].as_array().map(Vec::len), Some(4));
}

#[tokio::test]
async fn a_tool_round_feeds_its_results_back_to_the_model() {
    let server = MockServer::start().await;
    mount_script(&server, &[TOOL_CALLS, TEXT]).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let mut agent = agent(&server, dir.path(), 25);

    let events = run(&mut agent, "list files").await;

    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AssistantMessage { tool_calls, .. } if !tool_calls.is_empty() => {
                Some(tool_calls.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 2, "both calls of the reply are kept in order");

    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "call_a");
    assert!(results[0].1, "read_file succeeds: {}", results[0].2);
    assert!(results[0].2.contains("a.rs"), "{}", results[0].2);
    assert_eq!(results[1].0, "call_b");
    assert!(results[1].1, "bash succeeds: {}", results[1].2);

    assert_eq!(
        assistant_messages(&events),
        [("", false), ("Hello, world", false)],
        "an empty first reply, then the final answer"
    );

    let bodies = request_bodies(&server).await;
    assert_eq!(bodies.len(), 2, "one call per model step");
    let roles: Vec<&str> = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "tool", "tool"]);
    assert_eq!(bodies[1]["messages"][3]["tool_call_id"], "call_a");
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run -p overseer-core --test agent`
Expected: PASS, `2 tests run: 2 passed`. If the `roles` assertion fails, the assistant message is not reaching the conversation: `record` folds the event before emitting it.

- [ ] **Step 5: Run the gate**

Run: `just check-rust`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/core
git commit -m "feat(agent): run a turn of model calls and tool calls"
```

---

### Task 3: Failure, cap and cancellation paths

**Files:**
- Modify: `crates/core/tests/agent.rs` (tests appended)
- Modify: `crates/core/src/agent/mod.rs` only if a test exposes a defect

**Interfaces:**
- Consumes: everything from Task 2.
- Produces: proof that the turn ends for the right reason in each failure mode, and that a cancelled turn still answers every tool call it opened.

- [ ] **Step 1: Append the failure tests**

Add to `crates/core/tests/agent.rs`:

```rust
#[tokio::test]
async fn a_model_error_ends_the_turn_with_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&server, dir.path(), 25);

    let events = run(&mut agent, "hi").await;

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Error { message } if message.contains("500")
    )));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnd {
            reason: TurnEndReason::Error
        })
    );
}
```

```rust
#[tokio::test]
async fn a_failed_tool_result_goes_back_to_the_model_and_the_turn_continues() {
    let server = MockServer::start().await;
    let bad = include_str!("fixtures/llm/tool_calls.sse");
    mount_script(&server, &[bad, TEXT]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&server, dir.path(), 25);

    // `read_file` is called with {"path":"a.rs"} by the fixture, so remove the file instead:
    // the tool then fails with a readable message and the loop must still ask the model again.
    let events = run(&mut agent, "read a.rs").await;

    let results = tool_results(&events);
    assert!(!results[0].1, "a missing file is a failed result");
    assert!(results[0].2.contains("cannot read"), "{}", results[0].2);
    assert_eq!(assistant_messages(&events).len(), 2, "the model answered again");
}
```

```rust
#[tokio::test]
async fn the_step_cap_stops_a_model_that_keeps_calling_tools() {
    let server = MockServer::start().await;
    mount_script(&server, &[TOOL_CALLS, TOOL_CALLS]).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
    let mut agent = agent(&server, dir.path(), 2);

    let events = run(&mut agent, "loop").await;

    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnd {
            reason: TurnEndReason::MaxIterations
        })
    );
    assert_eq!(request_bodies(&server).await.len(), 2, "no third call");
    assert_eq!(tool_results(&events).len(), 4, "every call of both replies answered");
}
```

```rust
#[tokio::test]
async fn a_turn_cancelled_before_it_starts_calls_no_model() {
    let server = MockServer::start().await;
    mount_script(&server, &[TEXT]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&server, dir.path(), 25);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let mut events = Vec::new();
    let reason = agent.run_turn("hi", &mut events, &cancel).await;

    assert_eq!(reason, TurnEndReason::Cancelled);
    assert_eq!(events.first(), Some(&AgentEvent::UserMessage { text: "hi".into() }));
    assert!(request_bodies(&server).await.is_empty());
}
```


```rust
#[tokio::test]
async fn cancelling_during_a_tool_kills_it_and_answers_the_rest() {
    let server = MockServer::start().await;
    mount_script(&server, &[SLOW_TOOL]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&server, dir.path(), 25);
    let cancel = CancellationToken::new();
    let mut events = Vec::new();
    let mut turn = Box::pin(agent.run_turn("slow", &mut events, &cancel));
    tokio::select! {
        reason = &mut turn => panic!("turn finished before the cancel: {reason:?}"),
        () = tokio::time::sleep(std::time::Duration::from_millis(300)) => cancel.cancel(),
    }
    let reason = turn.await;
    assert_eq!(reason, TurnEndReason::Cancelled);
    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "the one call the reply asked for");
    assert!(!results[0].1);
    assert!(results[0].2.contains("cancelled"), "{}", results[0].2);
}
```


- [ ] **Step 2: Run the tests**
Run: `cargo nextest run -p overseer-core --test agent`
Expected: PASS, `7 tests run: 7 passed`.

- [ ] **Step 3: Run the gate**

Run: `just check-rust`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add crates/core
git commit -m "test(agent): cover failures, the step cap and cancellation"
```

---

### Task 4: The CLI

**Files:**
- Modify: `Cargo.toml`, `crates/cli/Cargo.toml`
- Create: `crates/cli/src/config.rs`, `crates/cli/tests/repl.rs`
- Modify: `crates/cli/src/main.rs`

**Interfaces:**
- Consumes: `Agent`, `AgentConfig`, `AgentEvent`, `EventSink`, `TurnEndReason`, `default_system_prompt` (Task 2); `Client`, `ClientConfig` (phase 1); `ToolRegistry` (phase 2).
- Produces: the `overseer-cli` binary. `Config::load(&Path) -> anyhow::Result<Config>`, `Config::api_key() -> Option<String>`, `Config::default_path() -> PathBuf`.

- [ ] **Step 1: Add the CLI dependencies**

In `Cargo.toml` `[workspace.dependencies]`:

```toml
toml = "1.2.2"
anyhow = "1.0.105"
```

In `crates/cli/Cargo.toml`:

```toml
[dependencies]
anyhow.workspace = true
overseer-core = { path = "../core" }
serde.workspace = true
tokio.workspace = true
tokio-util.workspace = true
toml.workspace = true
```

- [ ] **Step 2: Write the config, test first**

Create `crates/cli/src/config.rs` with only this test module, run `cargo nextest run -p overseer-cli`, expect a compile error on `Config`:

```rust
#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn parses_a_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "base_url = \"http://x/v1\"\nmodel = \"m\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.base_url, "http://x/v1");
        assert_eq!(config.model, "m");
        assert!(!config.send_reasoning);
    }

    #[test]
    fn rejects_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "base_url = \"u\"\nmodel = \"m\"\nextra = 1\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
```

- [ ] **Step 3: Write the config**

```rust
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub send_reasoning: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    #[must_use]
    pub fn api_key() -> Option<String> {
        std::env::var("OVERSEER_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
    }

    #[must_use]
    pub fn default_path() -> PathBuf {
        std::env::var_os("OVERSEER_CONFIG").map_or_else(
            || {
                std::env::var_os("HOME").map_or_else(
                    || PathBuf::from("overseer.toml"),
                    |home| PathBuf::from(home).join(".config/overseer/config.toml"),
                )
            },
            PathBuf::from,
        )
    }
}
```

Add `tempfile.workspace = true` to `[dev-dependencies]` of `crates/cli/Cargo.toml`, then run `cargo nextest run -p overseer-cli`: PASS, 2 tests.

- [ ] **Step 4: Write the REPL**

`crates/cli/src/main.rs`:

```rust
mod config;

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use config::Config;
use overseer_core::agent::{Agent, AgentConfig, AgentEvent, EventSink, default_system_prompt};
use overseer_core::llm::{Client, ClientConfig};
use overseer_core::tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// Prints a turn as it happens: text inline, one line per tool call and result.
struct Printer;

impl EventSink for Printer {
    fn emit(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolCallDelta {
                name: Some(name),
                arguments,
                ..
            } => println!("\n→ {name} {arguments}"),
            AgentEvent::ToolResult { ok, output, .. } => {
                let status = if ok { "ok" } else { "failed" };
                let first = output.lines().next().unwrap_or_default();
                println!("← {status}: {first}");
            }
            AgentEvent::AssistantMessage { .. } => println!(),
            AgentEvent::Error { message } => eprintln!("error: {message}"),
            AgentEvent::TurnEnd { reason } => println!("[turn end: {reason:?}]"),
            AgentEvent::UserMessage { .. }
            | AgentEvent::ReasoningDelta(_)
            | AgentEvent::ToolCallDelta { name: None, .. } => {}
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let path = Config::default_path();
    let config = Config::load(&path)?;
    let cwd = std::env::current_dir().context("cannot read the working directory")?;
    let mut client_config = ClientConfig::new(config.base_url, config.model);
    client_config.api_key = Config::api_key();
    client_config.send_reasoning = config.send_reasoning;
    let client = Client::new(client_config)?;
    let tools = ToolRegistry::new(&cwd);
    let mut agent = Agent::new(client, tools, AgentConfig::new(default_system_prompt(&cwd)));
    let cancel = CancellationToken::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.context("cannot read stdin")?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "exit" || text == "quit" {
            break;
        }
        agent.run_turn(text, &mut Printer, &cancel).await;
    }
    Ok(())
}
```

- [ ] **Step 5: Write the binary test**

`crates/cli/tests/repl.rs`:

```rust
use std::io::Write;
use std::process::{Command, Stdio};

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEXT: &str = include_str!("../../core/tests/fixtures/llm/text.sse");

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_runs_a_turn_from_stdin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(TEXT),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!("base_url = \"{}/v1\"\nmodel = \"m\"\n", server.uri()),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_overseer-cli"))
        .env("OVERSEER_CONFIG", &config)
        .env("OVERSEER_API_KEY", "sk-test")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hi\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("Hello, world"), "{stdout}");
    assert!(stdout.contains("[turn end: Done]"), "{stdout}");
}
```

Add `wiremock.workspace = true` to `[dev-dependencies]` of `crates/cli/Cargo.toml`.

- [ ] **Step 6: Run the tests**

Run: `cargo nextest run -p overseer-cli`
Expected: PASS, 3 tests (2 config, 1 binary). The binary test fails if the config path or `OVERSEER_API_KEY` is not picked up, so it also covers the wiring.

- [ ] **Step 7: Run the gate and commit**

Run: `just check-rust`

```bash
git add Cargo.toml Cargo.lock crates/cli
git commit -m "feat(cli): run turns from stdin against a configured model"
```

---

### Task 5: Record the turn contract in ADR 0001

**Files:**
- Modify: `docs/adr/0001-v0-architecture.md` (agent scope)
- Modify: `docs/plans/2026-11-24-overseer-v0.md` (phase 3 row)

- [ ] **Step 1: Add the turn rules**

After the `Tool arguments` section, add:

```markdown
### Turn loop

- A turn ends with exactly one `turn_end` and one of four reasons: `done` (the model answered without a tool), `cancelled`, `max_iterations`, `error`.
- At most 25 model calls per turn. The 26th never happens.
- Tool calls of one reply run in order, one at a time; every call gets a `tool_result` in the same order, including a cancelled turn ("cancelled before running").
- Cancellation is one token, shared by the model stream and the running tool.
- No compaction: a request the endpoint rejects for length ends the turn with `error`.
```

- [ ] **Step 2: Sharpen the phase 3 done-when**

Replace the phase 3 row with:

```markdown
| 3 | Agent loop: history rebuilt from events, iteration cap, one cancellation token reaching the model stream and a running bash; CLI over stdin | mock-LLM tests for text, tools, failures, the cap and cancellation; the CLI binary runs a turn against a scripted endpoint |
```

- [ ] **Step 3: Commit**

```bash
git add docs
git commit -m "docs: record the turn loop rules"
```

---

## Phase 3 done when

- `just check-rust` exits 0.
- `cargo nextest run -p overseer-core` reports the phase 1 and 2 suites plus 5 conversation tests and 7 agent tests.
- `cargo nextest run -p overseer-cli` reports 3 tests.
- The agent tests prove: a plain answer, a tool round with both results fed back, a failed tool result fed back, the step cap, an early cancel, a cancel during a tool (the command is killed and its result says so), and a model error.

## Known gap

- Cancellation **mid-stream** (while the model is still writing) is implemented by the `select!` in `complete`, but no automated test covers it: wiremock cannot stream slowly, and a hand-rolled TCP server was left out to keep the suite deterministic. The CLI is where it will be exercised first, by a real model. Add the slow-server test in phase 5, where SSE replay is already being tested.

Before Step 1, create `crates/core/tests/fixtures/llm/slow_tool.sse`: the shape of `tool_calls.sse` with one `bash` call whose arguments are `{"command":"sleep 30"}` and `finish_reason` `tool_calls`.
