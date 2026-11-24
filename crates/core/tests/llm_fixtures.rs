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
