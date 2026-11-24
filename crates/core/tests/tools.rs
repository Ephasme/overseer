//! Drives the registry the way the agent loop will: name, JSON arguments, a progress sink, a
//! cancellation token.

use std::time::{Duration, Instant};

use overseer_core::tools::{ToolOutcome, ToolRegistry};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn registry() -> (tempfile::TempDir, ToolRegistry) {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::new(dir.path());
    (dir, registry)
}

async fn call(registry: &ToolRegistry, name: &str, arguments: serde_json::Value) -> ToolOutcome {
    let mut sink = |_: &str| {};
    registry
        .call(
            name,
            &arguments.to_string(),
            &mut sink,
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn offers_the_four_built_in_tools_with_schemas() {
    let (_dir, registry) = registry();
    let definitions = registry.definitions();
    let names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    assert_eq!(names, ["bash"]);
    for definition in &definitions {
        assert_eq!(
            definition.parameters["type"], "object",
            "{}",
            definition.name
        );
        assert!(!definition.description.is_empty(), "{}", definition.name);
    }
    assert_eq!(definitions[0].parameters["required"], json!(["command"]));
}

#[tokio::test]
async fn an_unknown_tool_is_an_error_result_not_a_panic() {
    let (_dir, registry) = registry();
    let outcome = call(&registry, "teleport", json!({})).await;
    assert!(!outcome.ok);
    assert!(
        outcome.output.contains("unknown tool"),
        "{}",
        outcome.output
    );
    assert!(
        outcome.output.contains("available tools: bash"),
        "{}",
        outcome.output
    );
}

#[tokio::test]
async fn arguments_that_do_not_match_the_schema_are_an_error_result() {
    let (_dir, registry) = registry();
    let missing = call(&registry, "bash", json!({})).await;
    assert!(!missing.ok);
    assert!(
        missing.output.contains("invalid arguments"),
        "{}",
        missing.output
    );

    let extra = call(&registry, "bash", json!({ "command": "true", "lines": 3 })).await;
    assert!(!extra.ok, "unknown fields are rejected: {}", extra.output);
    assert!(extra.output.contains("unknown field"), "{}", extra.output);
}

#[tokio::test]
async fn bash_returns_stdout_and_success() {
    let (_dir, registry) = registry();
    let outcome = call(&registry, "bash", json!({ "command": "echo hello" })).await;
    assert!(outcome.ok, "{}", outcome.output);
    assert_eq!(outcome.output, "hello\n");
}

#[tokio::test]
async fn bash_interleaves_stderr_with_stdout_in_write_order() {
    let (_dir, registry) = registry();
    let outcome = call(
        &registry,
        "bash",
        json!({ "command": "echo one; echo two >&2; echo three" }),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert_eq!(outcome.output, "one\ntwo\nthree\n");
}

#[tokio::test]
async fn a_non_zero_exit_is_an_error_result_carrying_the_status() {
    let (_dir, registry) = registry();
    let outcome = call(&registry, "bash", json!({ "command": "echo bad; exit 3" })).await;
    assert!(!outcome.ok);
    assert!(outcome.output.contains("bad"), "{}", outcome.output);
    assert!(
        outcome.output.contains("[exit status: 3]"),
        "{}",
        outcome.output
    );
}

#[tokio::test]
async fn bash_runs_in_the_workdir_and_inherits_the_tool_environment() {
    let (dir, registry) = registry();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let outcome = call(
        &registry,
        "bash",
        json!({ "command": "pwd; echo $PAGER; echo $GIT_TERMINAL_PROMPT", "workdir": "sub" }),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    let lines: Vec<&str> = outcome.output.lines().collect();
    assert!(lines[0].ends_with("/sub"), "{}", outcome.output);
    assert_eq!(lines[1], "cat");
    assert_eq!(lines[2], "0");
}

#[tokio::test]
async fn bash_stdin_is_closed_so_cat_returns_at_once() {
    let (_dir, registry) = registry();
    let started = Instant::now();
    let outcome = call(
        &registry,
        "bash",
        json!({ "command": "cat", "timeout_seconds": 5 }),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert_eq!(outcome.output, "(no output)");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cat waited on stdin"
    );
}

#[tokio::test]
async fn bash_streams_each_line_to_the_progress_sink() {
    let (_dir, registry) = registry();
    let mut seen = Vec::new();
    let mut sink = |line: &str| seen.push(line.to_owned());
    let outcome = registry
        .call(
            "bash",
            &json!({ "command": "echo a; echo b" }).to_string(),
            &mut sink,
            &CancellationToken::new(),
        )
        .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert_eq!(seen, ["a", "b"]);
}

#[tokio::test]
async fn a_timeout_kills_the_whole_process_group() {
    let (_dir, registry) = registry();
    let started = Instant::now();
    let outcome = call(
        &registry,
        "bash",
        json!({ "command": "sleep 30 & echo $!; wait", "timeout_seconds": 1 }),
    )
    .await;
    assert!(!outcome.ok);
    assert!(
        outcome.output.contains("timed out after 1s"),
        "{}",
        outcome.output
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the call waited on sleep"
    );
    let background: i32 = outcome
        .output
        .lines()
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut gone = false;
    for _ in 0..40 {
        if !std::process::Command::new("kill")
            .arg("-0")
            .arg(background.to_string())
            .status()
            .unwrap()
            .success()
        {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "background child {background} survived the timeout");
}

#[tokio::test]
async fn cancelling_kills_the_command() {
    let (_dir, registry) = registry();
    let cancel = CancellationToken::new();
    let waiter = {
        let registry = registry.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut sink = |_: &str| {};
            registry
                .call("bash", r#"{"command":"sleep 30"}"#, &mut sink, &cancel)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    let outcome = waiter.await.unwrap();
    assert!(!outcome.ok);
    assert!(outcome.output.contains("cancelled"), "{}", outcome.output);
}

#[tokio::test]
async fn bash_output_is_capped_with_a_marker() {
    let (_dir, registry) = registry();
    let outcome = call(
        &registry,
        "bash",
        json!({ "command": "head -c 200000 /dev/zero | tr '\\0' 'a'" }),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert!(outcome.output.len() < 40_000, "{}", outcome.output.len());
    assert!(
        outcome.output.contains("bytes omitted"),
        "{}",
        &outcome.output[..80]
    );
}
