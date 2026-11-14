use crate::config;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: usize = 30_000;

/// Longest file body `read_file` returns, leaving room for its truncation note.
const MAX_READ_BYTES: usize = 28_000;

/// A command that outlives this is killed: one stuck worker must not stall the fleet forever.
const BASH_TIMEOUT: Duration = Duration::from_secs(300);

/// Process groups running right now, so quitting overseer does not leave a `cargo build` behind.
///
/// A group is deregistered before its command is reaped, so a recycled pid cannot be killed by name.
static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn groups() -> MutexGuard<'static, Vec<i32>> {
    GROUPS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn register(group: i32) {
    groups().push(group);
}

fn deregister(group: i32) {
    groups().retain(|live| *live != group);
}

/// Kills every command still running: the session calls this on its way out, under any frontend.
pub(crate) fn kill_running() {
    let running: Vec<i32> = groups().drain(..).collect();
    for group in running {
        let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
    }
}

/// Names of the worker's file and shell tools. The schema and the dispatch both use these.
pub(crate) const BASH: &str = "bash";
pub(crate) const READ_FILE: &str = "read_file";
pub(crate) const WRITE_FILE: &str = "write_file";

/// Output lines a tool result shows before the rest is only counted.
pub(crate) const PREVIEW_LINES: usize = 4;

/// Longest column of one previewed output line.
const PREVIEW_WIDTH: usize = 160;

/// What a tool result shows at a glance: a verdict, the head of its output, and what it hid.
///
/// The tool result itself is what the model reads, and stays the whole text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Preview {
    /// A one-line verdict such as `exit 3`; empty when the output speaks for itself.
    pub(crate) status: String,
    /// The first lines of output, newline separated.
    pub(crate) head: String,
    /// Output lines past `head`.
    pub(crate) more: usize,
    pub(crate) failed: bool,
}

/// Distils a tool result for display, so a chatty command does not flood the transcript.
pub(crate) fn preview(name: &str, output: &str) -> Preview {
    let mut failed = output.starts_with("error:");
    let mut status = String::new();
    let mut rows: Vec<&str> = Vec::new();
    for line in output.trim_end().lines() {
        if let Some(code) = line.strip_prefix("exit code: ") {
            failed |= code != "0";
            if failed {
                status = format!("exit {code}");
            }
            continue;
        }
        // The two stream labels describe the result's shape, not what the command did.
        if line == "stdout:" || line == "stderr:" {
            continue;
        }
        if line.starts_with("killed after ") {
            status = line.to_string();
            failed = true;
            continue;
        }
        rows.push(line);
    }
    let mut rows: Vec<&str> = rows.into_iter().skip_while(|row| row.trim().is_empty()).collect();
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    if status.is_empty() && name == READ_FILE && !rows.is_empty() {
        status = format!("{} lines", rows.len());
    }
    let head: Vec<String> = rows.iter().take(PREVIEW_LINES).map(|row| shorten(row, PREVIEW_WIDTH)).collect();
    Preview { status, head: head.join("\n"), more: rows.len().saturating_sub(PREVIEW_LINES), failed }
}

/// `text` in at most `width` characters, the cut marked with an ellipsis.
pub(crate) fn shorten(text: &str, width: usize) -> String {
    let mut short: String = text.chars().take(width).collect();
    if text.chars().count() > width {
        short.push('…');
    }
    short
}

/// Schemas of the file and shell tools every worker gets.
pub(crate) fn definitions() -> Value {
    json!([
        {"type": "function", "function": {
            "name": BASH,
            "description": format!("Run a shell command with `sh -c` in the working directory. Returns exit code, stdout and stderr. Stdin is closed; the command is killed after {} seconds.", BASH_TIMEOUT.as_secs()),
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}
        }},
        {"type": "function", "function": {
            "name": READ_FILE,
            "description": "Read a UTF-8 text file. Relative paths resolve against the working directory.",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }},
        {"type": "function", "function": {
            "name": WRITE_FILE,
            "description": "Create or overwrite a file with the given content. Creates missing parent directories. Relative paths resolve against the working directory.",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}
        }}
    ])
}

/// Arguments of each worker tool. `deny_unknown_fields` turns a wrong key into an error naming the right ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

/// Parses tool arguments, turning a serde failure into the text the model reads.
pub(crate) fn parse_args<'a, T: Deserialize<'a>>(arguments: &'a str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

/// Renders a tool outcome as the text the model reads: `error: ` for a failure, truncated otherwise.
pub(crate) fn outcome(result: Result<String, String>) -> String {
    truncate(result.unwrap_or_else(|e| format!("error: {e}")))
}

/// Runs one tool call inside `working_dir`; the returned string goes back to the model as the tool result.
pub(crate) fn execute(working_dir: &Path, name: &str, arguments: &str) -> String {
    execute_with(working_dir, name, arguments, BASH_TIMEOUT)
}

fn execute_with(working_dir: &Path, name: &str, arguments: &str, timeout: Duration) -> String {
    let result = match name {
        BASH => parse_args::<BashArgs>(arguments).and_then(|args| bash(working_dir, &args, timeout)),
        READ_FILE => parse_args::<ReadArgs>(arguments).and_then(|args| read_file(working_dir, &args)),
        WRITE_FILE => parse_args::<WriteArgs>(arguments).and_then(|args| write_file(working_dir, &args)),
        _ => Err(format!("unknown tool {name}")),
    };
    outcome(result)
}

fn bash(working_dir: &Path, args: &BashArgs, timeout: Duration) -> Result<String, String> {
    // Its own process group, so a timeout kills the command's children too: killing `sh` alone leaves a
    // grandchild holding the pipes open, and reading their output then blocks until it exits by itself.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&args.command)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    let group = i32::try_from(child.id()).map_err(|e| e.to_string())?;
    // Both pipes are drained by their own thread, so a chatty command cannot fill a pipe and stall.
    register(group);
    let mut stdout = drain(child.stdout.take());
    let mut stderr = drain(child.stderr.take());
    let waited = wait_with_deadline(&mut child, group, timeout);
    deregister(group);
    let (status, timed_out) = waited?;
    let (out, out_cut) = stdout.take_output();
    let (err, err_cut) = stderr.take_output();
    Ok(format_output(status, timed_out, timeout, (&out, out_cut), (&err, err_cut)))
}

/// Waits for `child` to exit, killing its process group once `timeout` elapses.
fn wait_with_deadline(child: &mut Child, group: i32, timeout: Duration) -> Result<(ExitStatus, bool), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok((status, false));
        }
        if Instant::now() >= deadline {
            let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
            let _ = child.kill();
            let status = child.wait().map_err(|e| e.to_string())?;
            return Ok((status, true));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn format_output(
    status: ExitStatus,
    timed_out: bool,
    timeout: Duration,
    (out, out_cut): (&[u8], bool),
    (err, err_cut): (&[u8], bool),
) -> String {
    let code = status.code().map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    let killed = if timed_out { format!("killed after {}s\n", timeout.as_secs()) } else { String::new() };
    let cut = if out_cut || err_cut {
        format!("\n[output truncated: only the first {MAX_OUTPUT_BYTES} bytes of each stream were kept]")
    } else {
        String::new()
    };
    format!(
        "{killed}exit code: {code}\nstdout:\n{}\nstderr:\n{}{cut}",
        String::from_utf8_lossy(out),
        String::from_utf8_lossy(err)
    )
}

/// A pipe reader that keeps draining in the background until the pipe closes, keeping a bounded head.
struct Drain(Option<thread::JoinHandle<(Vec<u8>, bool)>>);

impl Drain {
    /// The bytes kept, and whether the stream held more than were kept.
    fn take_output(&mut self) -> (Vec<u8>, bool) {
        self.0.take().and_then(|handle| handle.join().ok()).unwrap_or_default()
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Drain {
    Drain(pipe.map(|mut pipe| {
        thread::spawn(move || {
            // Storage stops at the cap; reading never does, because an unread pipe blocks its writer.
            let mut kept = Vec::with_capacity(8 * 1024);
            let mut scratch = [0_u8; 8 * 1024];
            let mut truncated = false;
            loop {
                match pipe.read(&mut scratch) {
                    Ok(0) | Err(_) => return (kept, truncated),
                    Ok(read) => {
                        let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                        let keep = read.min(room);
                        if let Some(head) = scratch.get(..keep) {
                            kept.extend_from_slice(head);
                        }
                        truncated |= keep < read;
                    }
                }
            }
        })
    }))
}

/// Reads a UTF-8 file, keeping the head of a file larger than the read budget.
fn read_file(working_dir: &Path, args: &ReadArgs) -> Result<String, String> {
    let full = working_dir.join(&args.path);
    let file = fs::File::open(&full).map_err(|e| config::io_error(&args.path, &e))?;
    let limit = u64::try_from(MAX_READ_BYTES).map_or(u64::MAX, |cap| cap.saturating_add(1));
    let mut bytes = Vec::with_capacity(8 * 1024);
    file.take(limit).read_to_end(&mut bytes).map_err(|e| config::io_error(&args.path, &e))?;
    let cut = bytes.len() > MAX_READ_BYTES;
    if cut {
        // The budget can land inside a multi-byte character: keep the longest valid prefix below it.
        let end = bytes
            .get(..MAX_READ_BYTES)
            .map_or(0, |head| std::str::from_utf8(head).map_or_else(|e| e.valid_up_to(), |_| head.len()));
        bytes.truncate(end);
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{}: not valid UTF-8", args.path))?;
    if cut {
        let shown = text.len();
        return Ok(format!(
            "{text}\n[truncated: first {shown} bytes shown; {MAX_READ_BYTES} is the read budget, not the file size]"
        ));
    }
    Ok(text)
}

fn write_file(working_dir: &Path, args: &WriteArgs) -> Result<String, String> {
    let full = working_dir.join(&args.path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| config::io_error(parent.display(), &e))?;
    }
    fs::write(&full, &args.content).map_err(|e| config::io_error(&args.path, &e))?;
    Ok(format!("wrote {} bytes to {}", args.content.len(), args.path))
}

/// Keeps the head and the tail of `s`: the head names what ran, the tail is where a command says why it failed.
fn truncate(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let total = s.len();
    let half = MAX_OUTPUT_BYTES / 2;
    let head_end = floor_boundary(&s, half);
    let tail_start = ceil_boundary(&s, total - half);
    let elided = tail_start.saturating_sub(head_end);
    let head = s.get(..head_end).unwrap_or_default();
    let tail = s.get(tail_start..).unwrap_or_default();
    format!("{head}\n[truncated: {elided} of {total} bytes elided]\n{tail}")
}

/// Largest char boundary of `s` at or below `at`.
fn floor_boundary(s: &str, at: usize) -> usize {
    let mut end = at.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Smallest char boundary of `s` at or above `at`.
fn ceil_boundary(s: &str, at: usize) -> usize {
    let mut start = at.min(s.len());
    while !s.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_runs_in_cwd_and_reports_exit_code_and_streams() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("marker"), "").unwrap();
        let out = execute(dir.path(), BASH, r#"{"command":"ls; echo oops >&2; exit 3"}"#);
        assert!(out.contains("exit code: 3"), "{out}");
        assert!(out.contains("stdout:\nmarker\n"), "{out}");
        assert!(out.contains("stderr:\noops\n"), "{out}");
    }

    #[test]
    fn bash_is_killed_past_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = execute_with(dir.path(), BASH, r#"{"command":"echo before; sleep 30"}"#, Duration::from_millis(200));
        assert!(out.starts_with("killed after 0s"), "{out}");
        assert!(out.contains("stdout:\nbefore\n"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn write_then_read_relative_path_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let write = json!({"path": "a/b/note.txt", "content": "héllo"}).to_string();
        assert_eq!(execute(dir.path(), WRITE_FILE, &write), "wrote 6 bytes to a/b/note.txt");
        assert_eq!(fs::read_to_string(dir.path().join("a/b/note.txt")).unwrap(), "héllo");
        assert_eq!(execute(dir.path(), READ_FILE, r#"{"path":"a/b/note.txt"}"#), "héllo");
    }

    #[test]
    fn errors_are_returned_not_raised() {
        let root = Path::new("/");
        assert!(execute(root, READ_FILE, r#"{"path":"/nonexistent/x"}"#).starts_with("error: /nonexistent/x:"));
        assert!(execute(root, BASH, "not json").starts_with("error: invalid arguments:"));
        assert!(execute(root, BASH, "{}").starts_with("error: invalid arguments: missing field `command`"));
        assert!(
            execute(root, BASH, r#"{"command":"ls","path":"x"}"#)
                .starts_with("error: invalid arguments: unknown field `path`, expected `command`")
        );
        assert_eq!(execute(root, "nope", "{}"), "error: unknown tool nope");
    }

    #[test]
    fn every_defined_tool_is_dispatched() {
        for tool in definitions().as_array().unwrap() {
            let name = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            assert!(
                !execute(Path::new("/"), name, "{}").starts_with("error: unknown tool"),
                "{name} is defined but not dispatched"
            );
        }
    }

    #[test]
    fn preview_separates_the_verdict_from_the_output() {
        let ok = preview(BASH, "exit code: 0\nstdout:\na\nb\nstderr:\n");
        assert_eq!(ok.status, "", "a success needs no chip");
        assert_eq!(ok.head, "a\nb");
        assert_eq!(ok.more, 0);
        assert!(!ok.failed);
        let failed = preview(BASH, "exit code: 2\nstdout:\nstderr:\nmissing\n");
        assert_eq!(failed.status, "exit 2");
        assert!(failed.failed);
        assert_eq!(failed.head, "missing");
        let file = (1..=9).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        let read = preview(READ_FILE, &file);
        assert_eq!(read.status, "9 lines");
        assert_eq!(read.head.lines().count(), PREVIEW_LINES);
        assert_eq!(read.more, 9 - PREVIEW_LINES);
    }

    #[test]
    fn truncation_keeps_the_head_and_the_tail() {
        let out = truncate("é".repeat(20_000)); // 40 000 bytes, 2 per char
        assert!(out.contains("[truncated: 10000 of 40000 bytes elided]"), "{out}");
        assert!(out.starts_with("éé"), "{out}");
        assert!(out.ends_with("éé"), "{out}");
        assert!(out.len() < MAX_OUTPUT_BYTES + 80, "{}", out.len());
    }

    #[test]
    fn a_chatty_command_is_capped_per_stream() {
        let dir = tempfile::tempdir().unwrap();
        let out = execute(dir.path(), BASH, r#"{"command":"yes | head -c 200000 >&2"}"#);
        assert!(out.contains("[output truncated: only the first 30000 bytes of each stream were kept]"), "{out}");
        assert!(out.len() < MAX_OUTPUT_BYTES + 200, "{}", out.len());
    }

    #[test]
    fn kill_running_stops_a_command_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let command = format!(r#"{{"command":"echo $$ > {}; sleep 30"}}"#, marker.display());
        let handle = thread::spawn(move || execute(dir.path(), BASH, &command));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "the command never started");
            thread::sleep(Duration::from_millis(10));
        }
        let killed = Instant::now();
        kill_running();
        let out = handle.join().unwrap();
        assert!(out.contains("killed by signal"), "{out}");
        assert!(killed.elapsed() < Duration::from_secs(10), "the command outlived the kill");
    }

    #[test]
    fn a_large_file_is_read_only_up_to_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big"), "a".repeat(100_000)).unwrap();
        let out = execute(dir.path(), READ_FILE, r#"{"path":"big"}"#);
        assert!(out.ends_with("28000 is the read budget, not the file size]"), "{}", &out[out.len() - 90..]);
        assert!(out.len() < MAX_OUTPUT_BYTES, "{}", out.len());
    }
}
