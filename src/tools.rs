use crate::config;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: usize = 30_000;

/// A command that outlives this is killed: one stuck worker must not stall the fleet forever.
const BASH_TIMEOUT: Duration = Duration::from_secs(300);

/// Names of the worker's file and shell tools. The schema and the dispatch both use these.
pub(crate) const BASH: &str = "bash";
pub(crate) const READ_FILE: &str = "read_file";
pub(crate) const WRITE_FILE: &str = "write_file";

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
    let mut stdout = drain(child.stdout.take());
    let mut stderr = drain(child.stderr.take());
    let (status, timed_out) = wait_with_deadline(&mut child, group, timeout)?;
    let out = stdout.take_output();
    let err = stderr.take_output();
    Ok(format_output(status, timed_out, timeout, &out, &err))
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

fn format_output(status: ExitStatus, timed_out: bool, timeout: Duration, out: &[u8], err: &[u8]) -> String {
    let code = status.code().map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    let killed = if timed_out { format!("killed after {}s\n", timeout.as_secs()) } else { String::new() };
    format!(
        "{killed}exit code: {code}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(out),
        String::from_utf8_lossy(err)
    )
}

/// A pipe reader that keeps draining in the background until the pipe closes.
struct Drain(Option<thread::JoinHandle<Vec<u8>>>);

impl Drain {
    fn take_output(&mut self) -> Vec<u8> {
        self.0.take().and_then(|handle| handle.join().ok()).unwrap_or_default()
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Drain {
    Drain(pipe.map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    }))
}

fn read_file(working_dir: &Path, args: &ReadArgs) -> Result<String, String> {
    fs::read_to_string(working_dir.join(&args.path)).map_err(|e| config::io_error(&args.path, &e))
}

fn write_file(working_dir: &Path, args: &WriteArgs) -> Result<String, String> {
    let full = working_dir.join(&args.path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| config::io_error(parent.display(), &e))?;
    }
    fs::write(&full, &args.content).map_err(|e| config::io_error(&args.path, &e))?;
    Ok(format!("wrote {} bytes to {}", args.content.len(), args.path))
}

fn truncate(mut s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let total = s.len();
    let mut end = MAX_OUTPUT_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    format!("{s}\n[truncated: first {end} of {total} bytes shown]")
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
    fn truncates_on_char_boundary() {
        let out = truncate("é".repeat(20_000)); // 40 000 bytes, 2 per char
        assert!(out.ends_with("[truncated: first 30000 of 40000 bytes shown]"), "{}", &out[out.len() - 60..]);
    }
}
