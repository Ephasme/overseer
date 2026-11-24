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
        .and(header("x-title", "overseer"))
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

#[test]
fn debug_never_prints_the_api_key() {
    let mut config = ClientConfig::new("https://example.test/v1", "m");
    config.api_key = Some("sk-secret-value".into());
    let rendered = format!("{config:?}");
    assert!(!rendered.contains("sk-secret-value"), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
}
