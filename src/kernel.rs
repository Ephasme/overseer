//! Persistent Python kernels behind the `python` tool.
//!
//! One kernel per (canonical working directory, runtime), shared by every agent of the session:
//! variables survive between calls. The bridge is a small script the runtime executes; it speaks
//! one JSON object per line on stdin and stdout, so the manager never parses anything else.
//!
//! The manager never blocks on a pipe write: a dedicated writer thread owns the child's stdin and
//! drains a channel, so a large `code` cannot stall the call loop past its own deadline.

use crate::tools::{MAX_OUTPUT_BYTES, deregister, spawn_grouped};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// The runtimes a kernel speaks. The name is both the program and the registry key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Runtime {
    Python,
    /// A program that does not exist, so a test can exercise the missing-runtime refusal.
    #[cfg(test)]
    Test,
}

impl Runtime {
    /// The program to launch.
    fn program(self) -> &'static str {
        match self {
            Self::Python => "python3",
            #[cfg(test)]
            Self::Test => "overseer-test-missing-runtime",
        }
    }

    /// The interpreter flags: `-E -s` keep Python from reading user env/site-packages while the
    /// working directory stays on `sys.path`, so code can import local modules.
    fn flags(self) -> &'static [&'static str] {
        match self {
            Self::Python => &["-E", "-s", "-u", "-c"],
            #[cfg(test)]
            Self::Test => &["-c"],
        }
    }

    /// The bridge script, embedded so the kernel needs no file on disk.
    fn bridge(self) -> &'static str {
        match self {
            Self::Python => PYTHON_BRIDGE,
            #[cfg(test)]
            Self::Test => "",
        }
    }
}

/// The bridge's answer: a finished result, a tool call, or a line the protocol cannot read.
#[derive(Debug)]
enum Frame {
    Result { ok: bool, output: String, error: Option<String> },
    Tool { name: String, arguments: String },
    Corrupt(String),
}

/// One frame as the wire carries it; unknown fields make the frame corrupt, not a guess.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFrame {
    kind: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    output: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Reads bridge frames off one line of stdout.
fn parse_frame(line: &str) -> Frame {
    let Ok(wire) = serde_json::from_str::<WireFrame>(line) else {
        return Frame::Corrupt("not a protocol frame".to_string());
    };
    match wire.kind.as_str() {
        "result" => Frame::Result { ok: wire.ok, output: wire.output, error: wire.error },
        "tool" => wire.name.map_or_else(
            || Frame::Corrupt("tool frame without a name".to_string()),
            |name| Frame::Tool { name, arguments: wire.arguments.unwrap_or_else(|| "{}".to_string()) },
        ),
        other => Frame::Corrupt(format!("unknown frame kind {other:?}")),
    }
}

struct Kernel {
    child: Mutex<Child>,
    /// The writer thread owns the child's stdin; this holds its sender. `None` once closed.
    stdin: Mutex<Option<Sender<String>>>,
    frames: Mutex<Option<Receiver<Frame>>>,
    /// The last stderr bytes, shown when the kernel dies so the model sees why.
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    group: i32,
    /// One call at a time: two writers on one stdin would interleave frames.
    call_lock: Mutex<()>,
}

impl Kernel {
    /// Queues one line for the writer thread. Never blocks: an unbounded channel absorbs a large
    /// `code`, and the writer's failure reaches the caller as the send error.
    fn write_line(&self, line: &str) -> Result<(), String> {
        self.stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .ok_or_else(|| "kernel stdin is closed".to_string())?
            .send(line.to_string())
            .map_err(|_| "kernel stdin is closed".to_string())
    }

    fn frames(&self) -> MutexGuard<'_, Option<Receiver<Frame>>> {
        self.frames.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Kills the process group and closes the stdin, so both threads end and the bridge sees EOF.
    fn kill(&self) {
        let _ = self.stdin.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let _ = Command::new("kill").arg("-9").arg(format!("-{}", self.group)).status();
        let _ = self.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner).wait();
    }

    /// The stderr tail as the model reads it.
    fn tail(&self) -> String {
        let tail = self.stderr_tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&tail).trim().to_string()
    }
}

/// Kernels keyed by (canonical working directory, runtime); the lock guards the map only.
type Kernels = HashMap<(PathBuf, Runtime), Arc<Kernel>>;

static KERNELS: LazyLock<Mutex<Kernels>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn kernels() -> MutexGuard<'static, Kernels> {
    KERNELS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The stderr pump: keeping the last bytes of a dying kernel is the only way to say why it died.
const STDERR_TAIL_BYTES: usize = 2048;

fn pump_stderr(stderr: impl Read + Send + 'static, tail: Arc<Mutex<Vec<u8>>>) {
    thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = [0u8; 512];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut tail = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    tail.extend_from_slice(buffer.get(..n).unwrap_or(&[]));
                    let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                    tail.drain(..excess);
                }
            }
        }
    });
}

/// Owns the child's stdin and drains the line channel, so a caller never blocks on a full pipe.
fn pump_stdin(mut stdin: ChildStdin, receiver: &Receiver<String>) {
    for line in receiver {
        let written =
            stdin.write_all(line.as_bytes()).and_then(|()| stdin.write_all(b"\n")).and_then(|()| stdin.flush());
        if written.is_err() {
            break;
        }
    }
    // Dropping the stdin closes the pipe: a live bridge reads EOF and exits.
}

/// The spawn failure as the model reads it: an absent runtime says so plainly.
fn spawn_error(runtime: Runtime, raw: &str) -> String {
    let raw = raw.to_lowercase();
    if raw.contains("no such file") || raw.contains("entity not found") || raw.contains("notfound") {
        format!("{} is not installed", runtime.program())
    } else {
        raw
    }
}

/// One kernel spawn under the caller's name: the registry lock never covers `Command::spawn`, so a
/// slow spawn cannot stall every other lookup.
/// Reads bytes off `reader` up to and including the next newline, discarding them: the tail of an
/// overlong frame line never enters memory.
fn drain_to_newline(reader: &mut BufReader<ChildStdout>) {
    let mut sink = [0u8; 1];
    loop {
        match reader.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) if sink[0] == b'\n' => break,
            Ok(_) => {}
        }
    }
}

fn spawn_kernel(working_dir: &Path, runtime: Runtime) -> Result<Arc<Kernel>, String> {
    let mut command = Command::new(runtime.program());
    command
        .args(runtime.flags())
        .arg(runtime.bridge())
        .current_dir(working_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let (child, group) = spawn_grouped(&mut command).map_err(|e| spawn_error(runtime, &e))?;
    let mut child = child;
    let stdin = child.stdin.take().ok_or_else(|| "kernel has no stdin".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "kernel has no stdout".to_string())?;
    let stderr = child.stderr.take().ok_or_else(|| "kernel has no stderr".to_string())?;
    let stderr_tail = Arc::new(Mutex::new(Vec::new()));
    pump_stderr(stderr, Arc::clone(&stderr_tail));
    let (lines, line_receiver) = channel();
    thread::spawn(move || pump_stdin(stdin, &line_receiver));
    let (sender, receiver) = channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        // The frame is assembled from small reads, so a blocking pipe delivers it as it arrives;
        // memory stays bounded because a line past the cap keeps its head and drains the rest.
        loop {
            let mut line = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let Ok(read) = reader.read(&mut buffer) else {
                    let _ = sender.send(Frame::Corrupt("stdout closed".to_string()));
                    return;
                };
                if read == 0 {
                    if line.is_empty() {
                        let _ = sender.send(Frame::Corrupt("kernel exited".to_string()));
                        return;
                    }
                    // EOF mid-line: emit what arrived, then the kernel is gone.
                    let text = String::from_utf8_lossy(&line);
                    let _ = sender.send(parse_frame(&text));
                    let _ = sender.send(Frame::Corrupt("kernel exited".to_string()));
                    return;
                }
                let chunk = buffer.get(..read).unwrap_or(&[]);
                if let Some(newline) = chunk.iter().position(|b| *b == b'\n') {
                    // Keep only the first MAX_OUTPUT_BYTES of the line; the rest of this
                    // chunk is already in hand, and whatever follows drains below.
                    let kept = MAX_OUTPUT_BYTES.saturating_sub(line.len()).min(newline);
                    line.extend_from_slice(chunk.get(..kept).unwrap_or(&[]));
                    let text = String::from_utf8_lossy(&line);
                    if sender.send(parse_frame(&text)).is_err() {
                        return;
                    }
                    break;
                }
                let kept = MAX_OUTPUT_BYTES.saturating_sub(line.len()).min(read);
                line.extend_from_slice(chunk.get(..kept).unwrap_or(&[]));
                if kept < read {
                    drain_to_newline(&mut reader);
                    let text = String::from_utf8_lossy(&line);
                    if sender.send(parse_frame(&text)).is_err() {
                        return;
                    }
                    break;
                }
            }
        }
    });
    let kernel = Arc::new(Kernel {
        child: Mutex::new(child),
        stdin: Mutex::new(Some(lines)),
        frames: Mutex::new(Some(receiver)),
        stderr_tail,
        group,
        call_lock: Mutex::new(()),
    });
    Ok(kernel)
}

fn get_or_spawn(working_dir: &Path, runtime: Runtime) -> Result<Arc<Kernel>, String> {
    let canonical = working_dir.canonicalize().unwrap_or_else(|_| working_dir.to_path_buf());
    let key = (canonical.clone(), runtime);
    let cached = kernels().get(&key).cloned();
    if let Some(kernel) = cached {
        // A kernel the machine already lost cannot serve this call: drop it and respawn.
        let alive = kernel
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_wait()
            .is_ok_and(|state| state.is_none());
        if alive {
            return Ok(kernel);
        }
        kill_kernel(&kernel);
        kernels().remove(&key);
    }
    let kernel = spawn_kernel(&canonical, runtime)?;
    kernels().insert(key, Arc::clone(&kernel));
    Ok(kernel)
}

/// Removes and kills one kernel, only when the registry still holds the one that failed: another
/// caller may already have replaced it with a fresh interpreter we must not kill.
fn remove_failed(canonical: &Path, runtime: Runtime, failed: &Arc<Kernel>) {
    let key = (canonical.to_path_buf(), runtime);
    let current = kernels().get(&key).filter(|live| Arc::ptr_eq(live, failed)).cloned();
    if let Some(kernel) = current {
        kernels().remove(&key);
        kernel.kill();
        deregister(kernel.group);
    }
}

/// Kills the kernel this caller holds, whatever the registry now says.
fn kill_kernel(kernel: &Kernel) {
    kernel.kill();
    deregister(kernel.group);
}

/// Kills every kernel still running: the session calls this on its way out.
pub(crate) fn kill_all() {
    for (_, kernel) in kernels().drain() {
        kernel.kill();
        deregister(kernel.group);
    }
}

/// How often the call loop wakes to check the deadline and the cancel flag.
const POLL: Duration = Duration::from_millis(25);

/// Takes the call lock, bounded by the same deadline and cancel flag as the call itself, so a
/// second caller cannot wait forever behind one that never returns.
fn lock_call<'a>(
    kernel: &'a Kernel,
    deadline: Instant,
    cancel: Option<&'a std::sync::atomic::AtomicBool>,
) -> Result<MutexGuard<'a, ()>, String> {
    loop {
        match kernel.call_lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(poison)) => return Ok(poison.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for a busy kernel".to_string());
        }
        if cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
            return Err("interrupted by overseer while waiting for a busy kernel".to_string());
        }
        thread::sleep(POLL);
    }
}

/// Reports a dead kernel with its stderr tail, after resetting it.
fn died(runtime: Runtime, cause: &str, kernel: &Arc<Kernel>, canonical: &Path) -> String {
    let tail = kernel.tail();
    remove_failed(canonical, runtime, kernel);
    format!("{} kernel died: {cause}; stderr: {tail}", runtime.program())
}

/// Runs `code` in the kernel for (canonical working directory, runtime), answering its tool calls
/// through `on_tool`, which returns that tool's result text or an error message.
/// An ordinary interpreter exception keeps the kernel and its state; a malformed frame, a write
/// failure, the deadline or the cancel flag kill it, and the next call starts a fresh interpreter.
pub(crate) fn call(
    working_dir: &Path,
    runtime: Runtime,
    code: &str,
    timeout: Duration,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    on_tool: &mut dyn FnMut(&str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let kernel = get_or_spawn(working_dir, runtime)?;
    let canonical = working_dir.canonicalize().unwrap_or_else(|_| working_dir.to_path_buf());
    let deadline = Instant::now() + timeout;
    let _call = lock_call(&kernel, deadline, cancel)?;
    let line = json!({"kind": "run", "code": code}).to_string();
    if let Err(e) = kernel.write_line(&line) {
        return Err(died(runtime, &e, &kernel, &canonical));
    }
    loop {
        let frame = {
            let guard = kernel.frames();
            match guard.as_ref() {
                Some(receiver) => match receiver.try_recv() {
                    Ok(frame) => Some(frame),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Frame::Corrupt("kernel exited".to_string())),
                },
                None => Some(Frame::Corrupt("kernel exited".to_string())),
            }
        };
        let Some(frame) = frame else {
            if Instant::now() >= deadline {
                kill_kernel(&kernel);
                remove_failed(&canonical, runtime, &kernel);
                break Err(format!("killed after {}s (kernel state reset)", timeout.as_secs()));
            }
            if cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
                kill_kernel(&kernel);
                remove_failed(&canonical, runtime, &kernel);
                break Err("interrupted by overseer (kernel state reset)".to_string());
            }
            thread::sleep(POLL);
            continue;
        };
        match frame {
            Frame::Result { ok, output, error } => {
                // The interpreter keeps running and its state keeps: a failed call is not a dead kernel.
                let output = truncate_note(&output, "output");
                break if ok {
                    Ok(output)
                } else {
                    Err(format!("{output}\n{}", error.unwrap_or_else(|| "unknown error".to_string())))
                };
            }
            Frame::Tool { name, arguments } => {
                // A stuck tool must not overrun the call's budget: give it what remains.
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    kill_kernel(&kernel);
                    remove_failed(&canonical, runtime, &kernel);
                    break Err(format!("killed after {}s (kernel state reset)", timeout.as_secs()));
                }
                let reply = match crate::tools::bounded_tool_call(on_tool, &name, &arguments, remaining, cancel) {
                    Ok(Ok(text)) => text,
                    Ok(Err(e)) => format!("error: {e}"),
                    Err(()) => {
                        kill_kernel(&kernel);
                        remove_failed(&canonical, runtime, &kernel);
                        break Err(format!("killed after {}s (kernel state reset)", timeout.as_secs()));
                    }
                };
                // Capped before the pipe: a too-large frame would fill it and block both ends.
                let line = json!({"kind": "tool-result", "output": truncate_note(&reply, "tool output")}).to_string();
                if let Err(e) = kernel.write_line(&line) {
                    break Err(died(runtime, &e, &kernel, &canonical));
                }
            }
            Frame::Corrupt(reason) => {
                kill_kernel(&kernel);
                remove_failed(&canonical, runtime, &kernel);
                break Err(format!(
                    "{} kernel returned a malformed frame ({reason}); its state was reset",
                    runtime.program()
                ));
            }
        }
    }
}

/// Caps one frame's payload, the same note the other tools use on a cut.
fn truncate_note(text: &str, label: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[{label} truncated: first {end} bytes shown]", text.get(..end).unwrap_or_default())
}

const PYTHON_BRIDGE: &str = r#"
import json, sys, io, traceback

REAL_OUT, REAL_ERR = sys.stdout, sys.stderr

def tool(name, arguments=None):
    if arguments is not None and not isinstance(arguments, str):
        arguments = json.dumps(arguments)
    REAL_OUT.write(json.dumps({"kind": "tool", "name": name, "arguments": arguments or "{}"}) + "\n")
    REAL_OUT.flush()
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line).get("output", "")

ns = {"tool": tool}

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except ValueError:
        continue
    if message.get("kind") != "run":
        continue
    out = io.StringIO()
    err = io.StringIO()
    old_out, old_err = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = out, err
    try:
        code = message.get("code", "")
        try:
            exec(code, ns)
            ok = True
            error = None
        except BaseException as e:
            ok = False
            error = "".join(traceback.format_exception_only(type(e), e))
    finally:
        sys.stdout, sys.stderr = old_out, old_err
    output = (out.getvalue() + err.getvalue()).rstrip()
    # Cap at the source: a giant `print` must not reach the manager whole, or the manager
    # would buffer the entire line before it can truncate.
    if len(output) > 28000:
        output = output[:28000] + "\n[output truncated: first 28000 bytes shown]"
    reply = {"kind": "result", "ok": ok, "output": output}
    if error:
        reply["error"] = error
    print(json.dumps(reply), flush=True)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::BASH_TIMEOUT;
    use std::sync::atomic::AtomicBool;

    fn dir() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    /// One call-scoped closure: coercion at the `&mut dyn FnMut` parameter needs no cast.
    macro_rules! no_tool {
        () => {
            &mut (|_: &str, _: &str| -> Result<String, String> { unreachable!("no tool call expected") })
        };
    }

    #[test]
    fn python_state_survives_between_calls() {
        let working = dir();
        call(&working, Runtime::Python, "x = 1", BASH_TIMEOUT, None, no_tool!()).unwrap();
        let out = call(&working, Runtime::Python, "print(x + 1)", BASH_TIMEOUT, None, no_tool!()).unwrap();
        assert_eq!(out, "2");
    }

    #[test]
    fn a_python_error_names_the_exception_and_keeps_the_state() {
        let working = dir();
        call(&working, Runtime::Python, "x = 7", BASH_TIMEOUT, None, no_tool!()).unwrap();
        let err = call(&working, Runtime::Python, "1 / 0", BASH_TIMEOUT, None, no_tool!()).unwrap_err();
        assert!(err.contains("ZeroDivisionError"), "{err}");
        let out = call(&working, Runtime::Python, "print(x + 1)", BASH_TIMEOUT, None, no_tool!()).unwrap();
        assert_eq!(out, "8", "an ordinary error must not reset the kernel");
    }

    #[test]
    fn a_bridge_tool_call_reaches_on_tool() {
        let working = dir();
        let calls = Mutex::new(Vec::new());
        let seen = &mut |name: &str, arguments: &str| -> Result<String, String> {
            calls.lock().unwrap().push((name.to_string(), arguments.to_string()));
            Ok("wrote the file".to_string())
        };
        let out = call(
            &working,
            Runtime::Python,
            "print(tool('write_file', {'path': 'a.txt', 'content': 'hi'}))",
            BASH_TIMEOUT,
            None,
            seen,
        )
        .unwrap();
        assert_eq!(out, "wrote the file");
        let calls = calls.into_inner().unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].0, "write_file");
        assert!(calls[0].1.contains("\"path\":\"a.txt\"") || calls[0].1.contains("\"path\": \"a.txt\""), "{calls:?}");
    }


    #[test]
    fn an_oversized_result_is_truncated_with_a_note() {
        let working = dir();
        let out = call(
            &working,
            Runtime::Python,
            &format!("print('x' * {})", MAX_OUTPUT_BYTES * 2),
            BASH_TIMEOUT,
            None,
            no_tool!(),
        )
        .unwrap();
        assert!(out.len() < MAX_OUTPUT_BYTES + 200, "{}", out.len());
        assert!(out.contains("[output truncated: first"), "{out}");
    }

    #[test]
    fn an_oversized_tool_reply_is_capped_before_the_pipe() {
        let working = dir();
        let seen = &mut |_: &str, _: &str| -> Result<String, String> { Ok("y".repeat(MAX_OUTPUT_BYTES * 2)) };
        // Without the cap the reply frame would fill the pipe and both ends would block.
        let out = call(
            &working,
            Runtime::Python,
            "print(tool('write_file', {'path': 'a', 'content': 'b'})[:40])",
            BASH_TIMEOUT,
            None,
            seen,
        )
        .unwrap();
        // Python slices the capped reply to 40 chars; without the cap the 60 000-char frame would
        // fill the pipe and both ends would block. The reply text reaching code is the truncated form.
        assert!(out.len() == 40, "{out}");
        assert!(out.len() < MAX_OUTPUT_BYTES + 200, "{}", out.len());
    }

    #[test]
    fn a_kernel_past_its_deadline_is_killed_and_reset() {
        let working = dir();
        let started = Instant::now();
        let err = call(
            &working,
            Runtime::Python,
            "import time; time.sleep(120)",
            Duration::from_millis(300),
            None,
            no_tool!(),
        )
        .unwrap_err();
        assert!(err.contains("killed after 0s (kernel state reset)"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10), "the kill took too long");
        let out = call(&working, Runtime::Python, "print('fresh')", BASH_TIMEOUT, None, no_tool!()).unwrap();
        assert_eq!(out, "fresh", "the reset left no state behind");
    }

    #[test]
    fn a_cancelled_kernel_call_is_killed_and_reset() {
        let working = dir();
        let cancel = AtomicBool::new(true);
        let err = call(&working, Runtime::Python, "x = 1", BASH_TIMEOUT, Some(&cancel), no_tool!()).unwrap_err();
        assert!(err.contains("interrupted by overseer"), "{err}");
        let out = call(&working, Runtime::Python, "print('fresh')", BASH_TIMEOUT, None, no_tool!()).unwrap();
        assert_eq!(out, "fresh");
    }

    #[test]
    fn a_caller_waits_at_most_its_own_deadline_behind_another() {
        let working = dir();
        let other = working.clone();
        // The slow call holds the lock far past the second caller's short budget.
        let slow = thread::spawn(move || {
            let _ =
                call(&other, Runtime::Python, "import time; time.sleep(120)", Duration::from_secs(5), None, no_tool!());
        });
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        // The same directory: a different one would reach a different kernel with no contention.
        let err = call(&working, Runtime::Python, "print('never')", Duration::from_millis(300), None, no_tool!())
            .unwrap_err();
        assert_eq!(err, "timed out waiting for a busy kernel");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        slow.join().unwrap();
    }

    #[test]
    fn a_missing_runtime_names_the_program() {
        // macOS says "entity not found"; Linux says "No such file or directory".
        assert_eq!(spawn_error(Runtime::Python, "entity not found"), "python3 is not installed");
        assert_eq!(spawn_error(Runtime::Python, "No such file or directory"), "python3 is not installed");
    }

    #[test]
    fn a_runtime_the_machine_lacks_is_refused_before_any_pipe() {
        let working = dir();
        let err = call(&working, Runtime::Test, "print(1)", BASH_TIMEOUT, None, no_tool!()).unwrap_err();
        assert_eq!(err, "overseer-test-missing-runtime is not installed");
    }

    #[test]
    fn a_malformed_frame_is_corrupt_not_a_guess() {
        assert!(matches!(parse_frame("not json"), Frame::Corrupt(_)));
        assert!(matches!(parse_frame(r#"{"kind": "ping"}"#), Frame::Corrupt(_)));
        assert!(matches!(parse_frame(r#"{"kind": "tool"}"#), Frame::Corrupt(_)), "a tool frame needs a name");
        assert!(matches!(
            parse_frame(r#"{"kind": "result", "ok": true, "output": "", "extra": 1}"#),
            Frame::Corrupt(_)
        ));
        match parse_frame(r#"{"kind": "tool", "name": "read_file"}"#) {
            Frame::Tool { name, arguments } => {
                assert_eq!(name, "read_file");
                assert_eq!(arguments, "{}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_truncated_note_holds_the_byte_budget() {
        let note = truncate_note(&"é".repeat(MAX_OUTPUT_BYTES), "output"); // 2 bytes per char
        assert!(note.contains("[output truncated: first "), "{note}");
        assert!(note.len() < MAX_OUTPUT_BYTES + 100, "{}", note.len());
    }
}
