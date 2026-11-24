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
    Client::new(ClientConfig::new(
        format!("{}/v1", server.uri()),
        "test-model",
    ))
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
            AgentEvent::ToolResult {
                call_id,
                ok,
                output,
            } => Some((call_id.as_str(), *ok, output.as_str())),
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

    assert_eq!(
        events.first(),
        Some(&AgentEvent::UserMessage { text: "hi".into() })
    );
    assert_eq!(assistant_messages(&events), [("Hello, world", false)]);
    assert_eq!(tool_results(&events), Vec::new());
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

    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "call_a");
    assert!(results[0].1, "read_file succeeds: {}", results[0].2);
    assert!(results[0].2.contains("fn main"), "{}", results[0].2);
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

#[tokio::test]
async fn a_failed_tool_result_goes_back_to_the_model_and_the_turn_continues() {
    let server = MockServer::start().await;
    mount_script(&server, &[TOOL_CALLS, TEXT]).await;
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(&server, dir.path(), 25);

    let events = run(&mut agent, "read a.rs").await;

    let results = tool_results(&events);
    assert!(!results[0].1, "a missing file is a failed result");
    assert!(results[0].2.contains("cannot read"), "{}", results[0].2);
    assert_eq!(
        assistant_messages(&events).len(),
        2,
        "the model answered again"
    );
}

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
    assert_eq!(tool_results(&events).len(), 4, "every call answered");
}

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
    assert_eq!(
        events.first(),
        Some(&AgentEvent::UserMessage { text: "hi".into() })
    );
    assert_eq!(request_bodies(&server).await, Vec::<Value>::new());
}

#[tokio::test]
async fn cancelling_during_a_tool_kills_it_and_records_its_result() {
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
    assert_eq!(results.len(), 1);
    assert!(!results[0].1);
    assert!(results[0].2.contains("cancelled"), "{}", results[0].2);
}
