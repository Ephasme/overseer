# Phase 2: tools and registry — implementation plan

> **For the implementing agent:** Execute one task at a time. Tick each checkbox
> (`- [ ]` → `- [x]`) as you finish a step, and stop to ask if any step is unclear —
> never guess.

**Goal:** the four built-in tools the model can call — `bash`, `read_file`, `write_file`, `edit_file` — behind one registry that turns every failure into a result the model can read.

**Architecture:** `crates/core/src/tools` holds one file per concern: `paths` resolves arguments against the session directory, `cap` bounds what the model reads, `bash` runs a command in its own process group with a timeout and a cancel path, `files` implements the three file tools, `registry` owns the JSON schemas and dispatches by name. `ToolRegistry::call` never returns an error: unknown tools, bad arguments and failing commands all come back as `ToolOutcome { ok: false, .. }`, which phase 3 turns into a `tool_result` event.

**Tech Stack:** Rust 1.100 (edition 2024), tokio (`process`, `fs`, `io-util`, `time`), tokio-util (`CancellationToken`), nix (`killpg`), serde / serde_json, tempfile (dev).

**Spec:** [ADR 0001](../adr/0001-v0-architecture.md) (bash tool contract, tool list, no sandbox), [rough plan](2026-11-24-overseer-v0.md) phase 2.

## Global Constraints

- Rust toolchain `1.100`, edition 2024, `unsafe_code = "forbid"`, clippy `pedantic` at warn with `-D warnings`. No `#[allow]`; no `unsafe`, which is why killing a process group goes through `nix::sys::signal::killpg` and not `libc`.
- No dependency without code that uses it, in `[dependencies]`, `[dev-dependencies]` or `[workspace.dependencies]`: `cargo shear` fails on all three. Each task adds only what its own code uses.
- Versions: `tokio` 1.54.1 (`macros`, `rt`, `process`, `io-util`, `time`, `fs`), `tokio-util` 0.7.20 (`rt`), `nix` 0.32.0 (`signal`, `process`), `tempfile` 3.27.0 (dev only), on top of the existing `serde` 1.0.229, `serde_json` 1.0.154.
- The gate is `just check-rust`. CI runs it unchanged.
- Unix only: the tool layer uses `tokio::process::Command::process_group` and `nix::sys::signal::killpg`. macOS and Linux are the supported platforms (ADR 0001's v0 targets); a Windows port needs its own process-group strategy.
- No logging, no approval step, no sandbox (ADR 0001). Absolute paths are honoured; relative ones join the session directory.
- Tool output handed to the model is capped at `30 * 1024` bytes, head and tail kept, marker giving the number of bytes dropped. Memory stays bounded while a command runs: the cap is applied to the stream, not to a fully buffered string.
- `bash` contract (ADR 0001): a fresh `bash -c` per call in its own process group, stdin `/dev/null`, `PAGER=cat`, `GIT_PAGER=cat`, `GIT_TERMINAL_PROMPT=0`, stdout and stderr interleaved in one pipe, timeout 120 s by default and at most 600 s, timeout and cancel kill the whole process group.
- Commit messages in English, Conventional Commits.

## File map

| Path | Responsibility | Task |
|---|---|---|
| `Cargo.toml`, `crates/core/Cargo.toml` | tokio features, tokio-util, nix, tempfile | 2 |
| `crates/core/src/lib.rs` | exports `tools` | 1 |
| `crates/core/src/tools/mod.rs` | `ToolOutcome` and the module wiring | 1 |
| `crates/core/src/tools/cap.rs` | `cap_output`, `OutputBuffer` | 1 |
| `crates/core/src/tools/paths.rs` | `resolve` | 2 |
| `crates/core/src/tools/bash.rs` | the `bash` tool | 2 |
| `crates/core/src/tools/registry.rs` | schemas and dispatch | 2, 3, 4 |
| `crates/core/src/tools/files.rs` | `read_file`, `write_file`, `edit_file` | 3, 4 |
| `crates/core/tests/tools.rs` | the registry driven like phase 3 will | 2, 3, 4 |
| `docs/adr/0001-v0-architecture.md` | tool arguments and the progress/cancel interface | 5 |

---

### Task 1: Dependencies, outcome type and output capping

**Files:**
- Modify: `crates/core/src/lib.rs`
- Create: `crates/core/src/tools/mod.rs`, `crates/core/src/tools/cap.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `overseer_core::tools::{ToolOutcome, OUTPUT_LIMIT, cap_output}`; `pub(crate)` `OutputBuffer { push(&mut self, chunk: &str), finish(self) -> String }`; the `tools` module with `mod cap;` wired.

- [ ] **Step 1: Wire the module and write `ToolOutcome`**

`crates/core/src/lib.rs`:

```rust
pub mod llm;
pub mod tools;
```

`crates/core/src/tools/mod.rs`:

```rust
//! The tools the model may call, and the registry that runs them.
//!
//! Every failure — unknown tool, arguments that do not match the schema, a missing file, a command
//! that exits non-zero — is returned to the model as a result with `ok: false`, never as an error.

mod cap;

pub use cap::{OUTPUT_LIMIT, cap_output};

/// What one tool call produced. `ok` becomes the `ok` field of the tool result event; `output` is
/// what the model reads, already capped by [`cap_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
}

impl ToolOutcome {
    #[must_use]
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
        }
    }

    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: message.into(),
        }
    }
}
```

`ToolRegistry` arrives in Task 2; the doc comment above already refers to it, so the first `cargo doc` warning about a broken intra-doc link is expected until then. Nothing else in this task depends on it.

- [ ] **Step 2: Write the failing cap tests**

Create `crates/core/src/tools/cap.rs` with only the `#[cfg(test)] mod tests` block from Step 4. Run `cargo nextest run -p overseer-core cap` and expect `error[E0432]: unresolved import super::{OUTPUT_LIMIT, OutputBuffer, cap_output}`.

- [ ] **Step 3: Write the module**

Complete `crates/core/src/tools/cap.rs` to exactly this:

```rust
/// Longest tool output handed back to the model, in bytes.
pub const OUTPUT_LIMIT: usize = 30 * 1024;

const HALF: usize = OUTPUT_LIMIT / 2;

/// Cuts `text` to [`OUTPUT_LIMIT`] bytes, keeping the first and last half around a marker that
/// gives the number of bytes dropped. Cutting happens on `char` boundaries.
#[must_use]
pub fn cap_output(text: &str) -> String {
    let mut buffer = OutputBuffer::default();
    buffer.push(text);
    buffer.finish()
}

/// Accumulates output while keeping memory bounded: the first [`HALF`] bytes and the last [`HALF`]
/// bytes, with the count of what fell between them. A command that never stops printing costs no
/// more memory than this.
#[derive(Debug, Default)]
pub(crate) struct OutputBuffer {
    head: String,
    tail: String,
    dropped: usize,
}

impl OutputBuffer {
    pub(crate) fn push(&mut self, chunk: &str) {
        let mut rest = chunk;
        if self.head.len() < HALF {
            let cut = floor_boundary(rest, HALF - self.head.len());
            self.head.push_str(&rest[..cut]);
            rest = &rest[cut..];
        }
        if rest.is_empty() {
            return;
        }
        self.tail.push_str(rest);
        if self.tail.len() > HALF {
            let cut = ceil_boundary(&self.tail, self.tail.len() - HALF);
            self.dropped += cut;
            self.tail.drain(..cut);
        }
    }

    pub(crate) fn finish(self) -> String {
        if self.dropped == 0 {
            self.head + &self.tail
        } else {
            format!(
                "{}\n… [{} bytes omitted] …\n{}",
                self.head, self.dropped, self.tail
            )
        }
    }
}

fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{OUTPUT_LIMIT, OutputBuffer, cap_output};

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(cap_output("hello\n"), "hello\n");
    }

    #[test]
    fn output_at_the_limit_is_untouched() {
        let text = "a".repeat(OUTPUT_LIMIT);
        assert_eq!(cap_output(&text), text);
    }

    #[test]
    fn long_output_keeps_head_and_tail_around_a_marker() {
        let text = format!(
            "{}{}{}",
            "H".repeat(OUTPUT_LIMIT),
            "x".repeat(5000),
            "T".repeat(OUTPUT_LIMIT)
        );
        let capped = cap_output(&text);
        assert!(capped.starts_with(&"H".repeat(100)), "head kept");
        assert!(capped.ends_with(&"T".repeat(100)), "tail kept");
        let dropped: usize = capped
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .expect("marker carries a byte count");
        assert_eq!(dropped, text.len() - OUTPUT_LIMIT, "count is exact");
        assert!(capped.len() <= OUTPUT_LIMIT + 64, "{}", capped.len());
    }

    #[test]
    fn cutting_never_splits_a_character() {
        let text = "é".repeat(OUTPUT_LIMIT);
        let capped = cap_output(&text);
        assert!(capped.starts_with('é') && capped.ends_with('é'));
        assert!(!capped.contains('\u{FFFD}'));
        assert!(capped.len() <= OUTPUT_LIMIT + 64, "{}", capped.len());
    }

    #[test]
    fn buffer_grows_by_chunks_and_stays_bounded() {
        let mut buffer = OutputBuffer::default();
        for _ in 0..1000 {
            buffer.push(&"y".repeat(1000));
        }
        let output = buffer.finish();
        assert!(output.len() <= OUTPUT_LIMIT + 64, "{}", output.len());
        assert!(output.contains("bytes omitted"), "{}", &output[..80]);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo nextest run -p overseer-core cap`
Expected: PASS, `5 tests run: 5 passed`.

- [ ] **Step 5: Run the gate**

Run: `just check-rust`
Expected: exit 0. This task adds no dependency: `cap.rs` uses only `std`, and `cargo shear` would fail on anything else.

- [ ] **Step 6: Commit**

```bash
git add crates/core
git commit -m "feat(tools): bound tool output and add the tool outcome type"
```

---

### Task 2: Path resolution, registry and the bash tool

**Files:**
- Modify: `Cargo.toml`, `crates/core/Cargo.toml`
- Create: `crates/core/src/tools/paths.rs`, `crates/core/src/tools/bash.rs`, `crates/core/src/tools/registry.rs`
- Create: `crates/core/tests/tools.rs`
- Modify: `crates/core/src/tools/mod.rs`

**Interfaces:**
- Consumes: `ToolOutcome`, `OutputBuffer` (Task 1), `crate::llm::ToolDefinition` (phase 1).
- Produces: `ToolRegistry::new(cwd)`, `ToolRegistry::definitions() -> Vec<ToolDefinition>`, `ToolRegistry::call(&self, name: &str, arguments: &str, progress: &mut (dyn FnMut(&str) + Send), cancel: &CancellationToken) -> ToolOutcome`; `pub(crate) fn resolve(cwd: &Path, path: &str) -> PathBuf`. Tasks 3 and 4 add arms to the same `call` and entries to the same `definitions`.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, inside `[workspace.dependencies]`, replace the existing `tokio` line and add three entries:

```toml
tokio = { version = "1.54.1", features = ["macros", "rt", "process", "io-util", "time", "fs"] }
tokio-util = { version = "0.7.20", features = ["rt"] }
nix = { version = "0.32.0", features = ["signal", "process"] }
tempfile = "3.27.0"
```

In `crates/core/Cargo.toml`, add to `[dependencies]`:

```toml
tokio-util.workspace = true
nix.workspace = true
```

and to `[dev-dependencies]`:

```toml
tempfile.workspace = true
```

`tokio` is already a dependency of `crates/core` through phase 1's dev-dependencies; move that line to `[dependencies]` if it is not there yet, since `bash.rs` uses it in the library.

- [ ] **Step 2: Write the path resolver**

`crates/core/src/tools/paths.rs`:

```rust
use std::path::{Path, PathBuf};

/// Absolute paths are used as given, relative ones join the session directory.
pub(crate) fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}
```

- [ ] **Step 3: Write the failing tests**

`crates/core/tests/tools.rs`:

```rust
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
```

Run: `cargo nextest run -p overseer-core --test tools`
Expected: FAIL, `error[E0432]: unresolved import overseer_core::tools::ToolRegistry`.

- [ ] **Step 4: Write the registry and the bash tool**

`crates/core/src/tools/registry.rs`:

```rust
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::ToolOutcome;
use super::bash;
use crate::llm::ToolDefinition;

/// The built-in tools, rooted at one session directory.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    cwd: PathBuf,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// The tools as the model sees them, in the order they are offered.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command in the session directory. Each call is a fresh \
                          bash, so nothing carries over between calls: chain with && or pass \
                          workdir. Returns stdout and stderr together and the exit status."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Command line to run." },
                    "workdir": {
                        "type": "string",
                        "description": "Directory to run in, relative to the session directory."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Kill the command after this many seconds (default 120, max 600)."
                    }
                },
                "required": ["command"]
            }),
        }]
    }

    /// Runs one call. `progress` receives the command's lines as they arrive (bash only); `cancel`
    /// kills a running command. Never returns an error: a failure is a `ToolOutcome` with `ok`
    /// false.
    pub async fn call(
        &self,
        name: &str,
        arguments: &str,
        progress: &mut (dyn FnMut(&str) + Send),
        cancel: &CancellationToken,
    ) -> ToolOutcome {
        match name {
            "bash" => match parse::<BashArgs>(arguments) {
                Ok(args) => bash::run(&self.cwd, args, progress, cancel).await,
                Err(message) => ToolOutcome::error(message),
            },
            other => {
                let definitions = self.definitions();
                let names: Vec<&str> = definitions
                    .iter()
                    .map(|definition| definition.name.as_str())
                    .collect();
                ToolOutcome::error(format!(
                    "unknown tool {other:?}; available tools: {}",
                    names.join(", ")
                ))
            }
        }
    }
}

fn parse<A: for<'de> Deserialize<'de>>(arguments: &str) -> Result<A, String> {
    serde_json::from_str(arguments).map_err(|error| {
        format!("invalid arguments: {error}. Expected a JSON object matching the tool's schema.")
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BashArgs {
    pub command: String,
    pub workdir: Option<String>,
    pub timeout_seconds: Option<u64>,
}
```

`crates/core/src/tools/bash.rs`:

```rust
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::ToolOutcome;
use super::cap::OutputBuffer;
use super::paths::resolve;
use super::registry::BashArgs;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// Runs one `bash -c` in its own process group, streaming lines to `progress` while collecting at
/// most [`super::OUTPUT_LIMIT`] bytes for the model.
enum End {
    Finished,
    TimedOut,
    Cancelled,
}

pub(crate) async fn run(
    cwd: &Path,
    args: BashArgs,
    progress: &mut (dyn FnMut(&str) + Send),
    cancel: &CancellationToken,
) -> ToolOutcome {
    let dir = args
        .workdir
        .as_deref()
        .map_or_else(|| cwd.to_path_buf(), |workdir| resolve(cwd, workdir));
    let timeout = timeout_for(args.timeout_seconds);

    let mut command = Command::new("bash");
    command
        .arg("-c")
        // The shell merges stderr into the stdout pipe, so the two interleave as the command wrote
        // them; two pipes cannot preserve that order.
        .arg(format!("exec 2>&1\n{}", args.command))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Own process group, so a timeout or cancel kills the whole tree, not just bash.
        .process_group(0)
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return ToolOutcome::error(format!("cannot start bash: {error}")),
    };
    let group = child.id().map(|pid| Pid::from_raw(pid.cast_signed()));
    let Some(stdout) = child.stdout.take() else {
        return ToolOutcome::error("bash produced no stdout pipe");
    };

    let mut lines = BufReader::new(stdout).lines();
    let mut output = OutputBuffer::default();

    // The read future borrows `output` and `progress`; the block ends that borrow before the
    // collected output is finished and returned.
    let end = {
        let read = read_lines(&mut lines, &mut output, progress);
        tokio::pin!(read);
        tokio::select! {
            () = &mut read => End::Finished,
            () = tokio::time::sleep(timeout) => End::TimedOut,
            () = cancel.cancelled() => End::Cancelled,
        }
    };

    if !matches!(end, End::Finished)
        && let Some(group) = group
    {
        let _ = killpg(group, Signal::SIGKILL);
    }
    let status = child.wait().await;
    let output = output.finish();

    match end {
        End::TimedOut => ToolOutcome::error(with_output(
            &output,
            &format!(
                "[timed out after {}s; process group killed]",
                timeout.as_secs()
            ),
        )),
        End::Cancelled => {
            ToolOutcome::error(with_output(&output, "[cancelled; process group killed]"))
        }
        End::Finished => match status {
            Ok(status) if status.success() => ToolOutcome::ok(with_output(&output, "")),
            Ok(status) => ToolOutcome::error(with_output(
                &output,
                &format!("[exit status: {}]", status.code().unwrap_or(-1)),
            )),
            Err(error) => ToolOutcome::error(with_output(
                &output,
                &format!("[cannot collect exit status: {error}]"),
            )),
        },
    }
}

async fn read_lines(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    output: &mut OutputBuffer,
    progress: &mut (dyn FnMut(&str) + Send),
) {
    while let Ok(Some(line)) = lines.next_line().await {
        progress(&line);
        output.push(&line);
        output.push("\n");
    }
}

fn with_output(output: &str, note: &str) -> String {
    match (output.is_empty(), note.is_empty()) {
        (true, true) => "(no output)".to_owned(),
        (true, false) => note.to_owned(),
        (false, true) => output.to_owned(),
        (false, false) => format!("{output}\n{note}"),
    }
}

fn timeout_for(seconds: Option<u64>) -> Duration {
    Duration::from_secs(
        seconds
            .unwrap_or(DEFAULT_TIMEOUT.as_secs())
            .clamp(1, MAX_TIMEOUT.as_secs()),
    )
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_TIMEOUT, MAX_TIMEOUT, timeout_for};

    #[test]
    fn default_timeout_is_two_minutes() {
        assert_eq!(timeout_for(None), DEFAULT_TIMEOUT);
    }

    #[test]
    fn requested_timeout_is_clamped_to_one_second_and_ten_minutes() {
        assert_eq!(timeout_for(Some(0)).as_secs(), 1);
        assert_eq!(timeout_for(Some(300)).as_secs(), 300);
        assert_eq!(timeout_for(Some(99_999)), MAX_TIMEOUT);
    }
}
```

In `crates/core/src/tools/mod.rs`, wire the two modules and export the registry; the file becomes:

```rust
//! The tools the model may call, and the registry that runs them.
//!
//! Every failure — unknown tool, arguments that do not match the schema, a missing file, a command
//! that exits non-zero — is returned to the model as a result with `ok: false`, never as an error
//! out of [`ToolRegistry::call`].

mod bash;
mod cap;
mod paths;
mod registry;

pub use cap::{OUTPUT_LIMIT, cap_output};
pub use registry::ToolRegistry;

/// What one tool call produced. `ok` becomes the `ok` field of the tool result event; `output` is
/// what the model reads, already capped by [`cap_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
}

impl ToolOutcome {
    #[must_use]
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: output.into(),
        }
    }

    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: message.into(),
        }
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo nextest run -p overseer-core`
Expected: PASS. The integration target reports `12 tests run: 12 passed`; the library adds 2 bash unit tests to Task 1's 5 cap tests, for `7 tests run: 7 passed` there. The timeout test takes about 1 s by design, and it fails if the background `sleep 30` survives the kill.

- [ ] **Step 6: Run the gate**

Run: `just check-rust`
Expected: exit 0.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/core
git commit -m "feat(tools): run bash in its own process group with timeout and cancel"
```

---

### Task 3: The file tools

**Files:**
- Create: `crates/core/src/tools/files.rs`
- Modify: `crates/core/src/tools/registry.rs` (three arms, three schema entries, three argument structs)
- Modify: `crates/core/src/tools/mod.rs` (`mod files;`)
- Modify: `crates/core/tests/tools.rs` (five tests appended)

**Interfaces:**
- Consumes: `ToolOutcome`, `cap_output` (Task 1), `resolve` (Task 2), the registry from Task 2.
- Produces: `files::read(&Path, ReadArgs)`, `files::write(&Path, WriteArgs)`, `files::edit(&Path, EditArgs)`, all `async` and all returning `ToolOutcome`; `definitions()` grows from one tool to four.

- [ ] **Step 1: Append the failing tests**

Append to `crates/core/tests/tools.rs`:

```rust
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
```

Run: `cargo nextest run -p overseer-core --test tools`
Expected: FAIL: `read_file`, `write_file` and `edit_file` answer "unknown tool", so the first assertion on `whole.output` fails.

- [ ] **Step 2: Write the file tools**

Create `crates/core/src/tools/files.rs`:

```rust
use std::fmt::Write as _;
use std::path::Path;

use tokio::fs;

use super::ToolOutcome;
use super::cap::cap_output;
use super::paths::resolve;
use super::registry::{EditArgs, ReadArgs, WriteArgs};

pub(crate) async fn read(cwd: &Path, args: ReadArgs) -> ToolOutcome {
    let path = resolve(cwd, &args.path);
    match fs::read(&path).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            ToolOutcome::ok(cap_output(&number_lines(
                &text,
                args.start_line,
                args.end_line,
            )))
        }
        Err(error) => ToolOutcome::error(format!("cannot read {}: {error}", path.display())),
    }
}

pub(crate) async fn write(cwd: &Path, args: WriteArgs) -> ToolOutcome {
    let path = resolve(cwd, &args.path);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = fs::create_dir_all(parent).await
    {
        return ToolOutcome::error(format!("cannot create {}: {error}", parent.display()));
    }
    match fs::write(&path, &args.content).await {
        Ok(()) => ToolOutcome::ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            path.display()
        )),
        Err(error) => ToolOutcome::error(format!("cannot write {}: {error}", path.display())),
    }
}

pub(crate) async fn edit(cwd: &Path, args: EditArgs) -> ToolOutcome {
    let path = resolve(cwd, &args.path);
    if args.old_string.is_empty() {
        return ToolOutcome::error("old_string must not be empty");
    }
    let text = match fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) => {
            return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
        }
    };
    match text.matches(&args.old_string).count() {
        0 => ToolOutcome::error(format!(
            "old_string not found in {}; read the file and copy the text exactly",
            path.display()
        )),
        1 => {
            let edited = text.replacen(&args.old_string, &args.new_string, 1);
            match fs::write(&path, edited).await {
                Ok(()) => ToolOutcome::ok(format!("edited {}", path.display())),
                Err(error) => {
                    ToolOutcome::error(format!("cannot write {}: {error}", path.display()))
                }
            }
        }
        count => ToolOutcome::error(format!(
            "old_string appears {count} times in {}; include surrounding lines to make it unique",
            path.display()
        )),
    }
}

/// Numbers the selected lines `  12\ttext`, padding to the width of the last line number.
fn number_lines(text: &str, start: Option<usize>, end: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return String::new();
    }
    let first = start.unwrap_or(1).clamp(1, total);
    let last = end.unwrap_or(total).clamp(1, total);
    if last < first {
        return format!("[no lines: start_line {first} is after end_line {last}]");
    }
    let width = last.to_string().len();
    let mut out = String::new();
    for (offset, line) in lines[first - 1..last].iter().enumerate() {
        writeln!(out, "{:>width$}\t{line}", first + offset, width = width)
            .expect("writing to a String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::number_lines;

    #[test]
    fn numbers_every_line_by_default() {
        assert_eq!(number_lines("a\nb\n", None, None), "1\ta\n2\tb\n");
    }

    #[test]
    fn pads_to_the_width_of_the_last_number() {
        let mut text = String::new();
        for n in 1..=12 {
            writeln!(text, "l{n}").expect("writing to a String cannot fail");
        }
        let numbered = number_lines(&text, Some(9), Some(12));
        assert_eq!(numbered, " 9\tl9\n10\tl10\n11\tl11\n12\tl12\n");
    }

    #[test]
    fn selects_a_range_and_clamps_it() {
        assert_eq!(number_lines("a\nb\nc\n", Some(2), Some(99)), "2\tb\n3\tc\n");
        assert_eq!(number_lines("a\nb\nc\n", Some(0), Some(1)), "1\ta\n");
    }

    #[test]
    fn reports_an_inverted_range() {
        assert_eq!(
            number_lines("a\nb\nc\n", Some(3), Some(2)),
            "[no lines: start_line 3 is after end_line 2]"
        );
    }

    #[test]
    fn an_empty_file_numbers_to_nothing() {
        assert_eq!(number_lines("", None, None), "");
    }
}
```

- [ ] **Step 3: Replace the registry**

Replace `crates/core/src/tools/registry.rs` with this version, which adds the three arms, their schemas and their argument structs:

```rust
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::ToolOutcome;
use super::bash;
use super::files;
use crate::llm::ToolDefinition;

/// The built-in tools, rooted at one session directory.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    cwd: PathBuf,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// The tools as the model sees them, in the order they are offered.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "bash".into(),
                description: "Run a shell command in the session directory. Each call is a fresh \
                              bash, so nothing carries over between calls: chain with && or pass \
                              workdir. Returns stdout and stderr together and the exit status."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Command line to run." },
                        "workdir": {
                            "type": "string",
                            "description": "Directory to run in, relative to the session directory."
                        },
                        "timeout_seconds": {
                            "type": "integer",
                            "description": "Kill the command after this many seconds (default 120, max 600)."
                        }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read a text file with line numbers. Use start_line and end_line to \
                              read part of a large file."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "start_line": { "type": "integer", "description": "First line, 1-based." },
                        "end_line": { "type": "integer", "description": "Last line, inclusive." }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "write_file".into(),
                description: "Write a whole file, creating parent directories. Overwrites."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolDefinition {
                name: "edit_file".into(),
                description: "Replace one exact string in a file. old_string must appear exactly \
                              once; read the file first. Use write_file to create or replace a file."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string" },
                        "new_string": { "type": "string" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
        ]
    }

    /// Runs one call. `progress` receives the command's lines as they arrive (bash only); `cancel`
    /// kills a running command. Never returns an error: a failure is a `ToolOutcome` with `ok`
    /// false.
    pub async fn call(
        &self,
        name: &str,
        arguments: &str,
        progress: &mut (dyn FnMut(&str) + Send),
        cancel: &CancellationToken,
    ) -> ToolOutcome {
        match name {
            "bash" => match parse::<BashArgs>(arguments) {
                Ok(args) => bash::run(&self.cwd, args, progress, cancel).await,
                Err(message) => ToolOutcome::error(message),
            },
            "read_file" => match parse::<ReadArgs>(arguments) {
                Ok(args) => files::read(&self.cwd, args).await,
                Err(message) => ToolOutcome::error(message),
            },
            "write_file" => match parse::<WriteArgs>(arguments) {
                Ok(args) => files::write(&self.cwd, args).await,
                Err(message) => ToolOutcome::error(message),
            },
            "edit_file" => match parse::<EditArgs>(arguments) {
                Ok(args) => files::edit(&self.cwd, args).await,
                Err(message) => ToolOutcome::error(message),
            },
            other => {
                let definitions = self.definitions();
                let names: Vec<&str> = definitions
                    .iter()
                    .map(|definition| definition.name.as_str())
                    .collect();
                ToolOutcome::error(format!(
                    "unknown tool {other:?}; available tools: {}",
                    names.join(", ")
                ))
            }
        }
    }
}

fn parse<A: for<'de> Deserialize<'de>>(arguments: &str) -> Result<A, String> {
    serde_json::from_str(arguments).map_err(|error| {
        format!("invalid arguments: {error}. Expected a JSON object matching the tool's schema.")
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BashArgs {
    pub command: String,
    pub workdir: Option<String>,
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditArgs {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
}
```

- [ ] **Step 4: Register the module**

In `crates/core/src/tools/mod.rs`, add `mod files;` after `mod cap;`. The module order is alphabetical: `bash`, `cap`, `files`, `paths`, `registry`.

- [ ] **Step 5: Run the tests**

Run: `cargo nextest run -p overseer-core`
Expected: PASS. The integration target reports `17 tests run: 17 passed`; the library reports `12 tests run: 12 passed` (5 cap, 2 bash, 5 line numbering).

- [ ] **Step 6: Run the gate**

Run: `just check-rust`
Expected: exit 0.

- [ ] **Step 7: Commit**

```bash
git add crates/core
git commit -m "feat(tools): read, write and edit files"
```

---

### Task 4: Record the tool arguments in ADR 0001

**Files:**
- Modify: `docs/adr/0001-v0-architecture.md` (after the `bash tool` section)
- Modify: `docs/plans/2026-11-24-overseer-v0.md` (phase 2 row)

**Interfaces:**
- Consumes: the registry's schemas and `call` signature from Tasks 2 and 3.
- Produces: the spec phase 3 (agent loop), phase 5 (SSE) and phase 7 (UI) read for tool names, arguments and the progress/cancel contract. Nothing reads it at runtime.

- [ ] **Step 1: Add the arguments to ADR 0001**

After the `### bash tool` section, insert:

```markdown
### Tool arguments

- `bash`: `command` (required), `workdir` (resolved against the session cwd), `timeout_seconds` (default 120, clamped to 1–600).
- `read_file`: `path`, `start_line`, `end_line` (1-based, inclusive; the whole file by default). Output is numbered lines, capped like every tool output.
- `write_file`: `path`, `content`; parent directories are created.
- `edit_file`: `path`, `old_string`, `new_string`; `old_string` must occur exactly once, or the call fails without writing.
- Unknown fields are rejected, so a model typo comes back as an error result instead of being silently ignored.
- Every failure — unknown tool, arguments that do not match the schema, a missing file, a non-zero exit — is a tool result with `ok: false`. The registry never returns an error.
- A call takes a progress sink, which receives each command line as it is printed (the `tool_progress` event, phase 5), and a cancellation token, which kills a running command's process group.
```

- [ ] **Step 2: Sharpen the phase 2 done-when**

In `docs/plans/2026-11-24-overseer-v0.md`, replace the phase 2 row with:

```markdown
| 2 | Tools + registry: JSON schemas, errors returned to the model as results, bash timeout kills the whole process group, output truncated | unit tests per tool; a timeout is proven to kill the background child, not just bash |
```

- [ ] **Step 3: Check the ADR reads as one argument**

Read the `Agent scope`, `bash tool` and `Tool arguments` sections together. Expected: no argument named in two places with two meanings, and the progress/cancel contract stated once.

- [ ] **Step 4: Commit**

```bash
git add docs
git commit -m "docs: record the tool arguments and the progress and cancel contract"
```

---

## Phase 2 done when

- `just check-rust` exits 0: clippy pedantic `-D warnings`, nextest `--locked`, cargo-shear, cargo-deny.
- `cargo nextest run -p overseer-core` reports 29 tests: 12 library (5 output capping, 2 timeout arithmetic, 5 line numbering) and 17 registry tests.
- The registry test suite proves, on a real process: stdout and stderr interleaved in write order, a non-zero exit carrying its status, a workdir and the tool environment applied, stdin closed, lines reaching the progress sink, output capped at 30 KB with a marker, a timeout killing the whole process group (the background child is gone), and a cancel doing the same.
- No test depends on a network or a model.
