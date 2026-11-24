//! A scripted OpenAI-compatible endpoint, so the loop can be driven without a real model.
//!
//! Every request is answered with one of the fixtures in `crates/core/tests/fixtures/llm`:
//! `tool_calls.sse` when the conversation still needs a tool round (the request's user text
//! contains `@tool`), `text.sse` otherwise or once a tool result is in the conversation.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const DEFAULT_PORT: u16 = 8788;
const DEFAULT_FIXTURES: &str = "crates/core/tests/fixtures/llm";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut port = DEFAULT_PORT;
    let mut fixtures = PathBuf::from(DEFAULT_FIXTURES);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().context("--port needs a value")?.parse()?,
            "--fixtures" => fixtures = args.next().context("--fixtures needs a path")?.into(),
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    println!(
        "fake model on http://127.0.0.1:{port}, fixtures in {}",
        fixtures.display()
    );
    loop {
        let (stream, _) = listener.accept().await?;
        let fixtures = fixtures.clone();
        tokio::spawn(async move {
            if let Err(error) = serve(stream, &fixtures).await {
                eprintln!("fake model: {error:#}");
            }
        });
    }
}

async fn serve(mut stream: TcpStream, fixtures: &std::path::Path) -> Result<()> {
    let body = read_request(&mut stream).await?;
    let fixture = choose(&body);
    let sse = std::fs::read_to_string(fixtures.join(fixture))?;
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        sse.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(sse.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Reads the request line, headers and `content-length` body.
async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(body_start) = find(&buffer, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..body_start]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            if buffer.len() >= body_start + 4 + length {
                break;
            }
        }
        if buffer.len() > 4 * 1024 * 1024 {
            anyhow::bail!("request larger than 4 MiB");
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A tool round while the request asks for one and no result has come back yet.
fn choose(request: &str) -> &'static str {
    let wants_tool = request.contains("@tool");
    let has_result = request.contains("\"role\":\"tool\"");
    if wants_tool && !has_result {
        "tool_calls.sse"
    } else {
        "text.sse"
    }
}
