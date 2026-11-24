//! Runs the binary the way a person does: config file, stdin, stdout.

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
    child.stdin.take().unwrap().write_all(b"hi\n").unwrap();
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
