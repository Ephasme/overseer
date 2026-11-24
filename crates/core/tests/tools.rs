//! Drives the registry the way the agent loop will: name, JSON arguments, a progress sink, a
//! cancellation token.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use overseer_core::tools::{OUTPUT_LIMIT, ToolOutcome, ToolRegistry};
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
    assert_eq!(names, ["bash", "read_file", "write_file", "edit_file"]);
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

#[tokio::test]
async fn read_file_numbers_lines_and_selects_a_range() {
    let (dir, registry) = registry();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let whole = call(&registry, "read_file", json!({ "path": "a.txt" })).await;
    assert_eq!(whole.output, "1\tone\n2\ttwo\n3\tthree\n");
    let range = call(
        &registry,
        "read_file",
        json!({ "path": "a.txt", "start_line": 2, "end_line": 3 }),
    )
    .await;
    assert_eq!(range.output, "2\ttwo\n3\tthree\n");
}

#[tokio::test]
async fn reading_a_missing_file_is_an_error_result() {
    let (_dir, registry) = registry();
    let outcome = call(&registry, "read_file", json!({ "path": "nope.txt" })).await;
    assert!(!outcome.ok);
    assert!(outcome.output.contains("cannot read"), "{}", outcome.output);
}

#[tokio::test]
async fn write_file_creates_parent_directories() {
    let (dir, registry) = registry();
    let outcome = call(
        &registry,
        "write_file",
        json!({ "path": "a/b/c.txt", "content": "hi" }),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
        "hi"
    );
    assert!(
        outcome.output.contains("wrote 2 bytes"),
        "{}",
        outcome.output
    );
}

#[tokio::test]
async fn edit_file_replaces_one_exact_occurrence() {
    let (dir, registry) = registry();
    std::fs::write(dir.path().join("a.txt"), "let x = 1;\nlet y = 2;\n").unwrap();
    let outcome = call(
        &registry,
        "edit_file",
        json!({ "path": "a.txt", "old_string": "let y = 2;", "new_string": "let y = 3;" }),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "let x = 1;\nlet y = 3;\n"
    );
}

#[tokio::test]
async fn edit_file_refuses_a_missing_or_ambiguous_match() {
    let (dir, registry) = registry();
    std::fs::write(dir.path().join("a.txt"), "same\nsame\n").unwrap();
    let missing = call(
        &registry,
        "edit_file",
        json!({ "path": "a.txt", "old_string": "other", "new_string": "x" }),
    )
    .await;
    assert!(!missing.ok);
    assert!(missing.output.contains("not found"), "{}", missing.output);
    let ambiguous = call(
        &registry,
        "edit_file",
        json!({ "path": "a.txt", "old_string": "same", "new_string": "x" }),
    )
    .await;
    assert!(!ambiguous.ok);
    assert!(
        ambiguous.output.contains("appears 2 times"),
        "{}",
        ambiguous.output
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "same\nsame\n",
        "a refused edit writes nothing"
    );
}

#[tokio::test]
async fn reading_a_huge_file_stays_bounded() {
    let (dir, registry) = registry();
    let mut text = String::new();
    for line in 0..40_000 {
        writeln!(text, "line {line} with enough text to matter").expect("writing to a String");
    }
    std::fs::write(dir.path().join("big.txt"), &text).unwrap();
    let outcome = call(&registry, "read_file", json!({ "path": "big.txt" })).await;
    assert!(outcome.ok, "{}", outcome.output);
    assert!(
        outcome.output.len() <= OUTPUT_LIMIT + 64,
        "{}",
        outcome.output.len()
    );
    assert!(outcome.output.starts_with("    1\tline 0"), "head kept");
    assert!(
        outcome.output.contains("bytes omitted"),
        "tail marker present"
    );
}

#[tokio::test]
async fn dropping_the_call_kills_the_process_group() {
    let (_dir, registry) = registry();
    let cancel = CancellationToken::new();
    let mut seen: Vec<String> = Vec::new();
    {
        let mut sink = |line: &str| seen.push(line.to_owned());
        let mut call = Box::pin(registry.call(
            "bash",
            r#"{"command":"echo $$; sleep 30"}"#,
            &mut sink,
            &cancel,
        ));
        tokio::select! {
            outcome = &mut call => panic!("finished: {outcome:?}"),
            () = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
    }
    let pid: i32 = seen
        .iter()
        .find_map(|line| line.trim().parse().ok())
        .expect("the command printed its pid");
    let mut gone = false;
    for _ in 0..40 {
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success();
        if !alive {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "process group {pid} survived the drop");
}
