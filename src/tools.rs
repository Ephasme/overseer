use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAX_OUTPUT_BYTES: usize = 30_000;

/// A command that outlives this is killed: one stuck worker must not stall the fleet forever.
pub(crate) const BASH_TIMEOUT: Duration = Duration::from_secs(300);

/// Schemas of the file and shell tools every worker gets.
pub(crate) fn definitions() -> Value {
    json!([
        {"type": "function", "function": {
            "name": "bash",
            "description": "Run a shell command with `sh -c` in the working directory. Returns exit code, stdout and stderr. Stdin is closed; the command is killed after 300 seconds.",
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}
        }},
        {"type": "function", "function": {
            "name": "read_file",
            "description": "Read a UTF-8 text file. Relative paths resolve against the repository directory.",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }},
        {"type": "function", "function": {
            "name": "write_file",
            "description": "Create or overwrite a file with the given content. Creates missing parent directories. Relative paths resolve against the repository directory.",
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

/// Runs one tool call inside `cwd`; the returned string goes back to the model as the tool result.
pub(crate) fn execute(cwd: &Path, name: &str, arguments: &str) -> String {
    execute_with(cwd, name, arguments, BASH_TIMEOUT)
}

fn execute_with(cwd: &Path, name: &str, arguments: &str, timeout: Duration) -> String {
    let result = match name {
        "bash" => parse::<BashArgs>(arguments).and_then(|args| bash(cwd, &args, timeout)),
        "read_file" => parse::<ReadArgs>(arguments).and_then(|args| read_file(cwd, &args)),
        "write_file" => parse::<WriteArgs>(arguments).and_then(|args| write_file(cwd, &args)),
        _ => Err(format!("unknown tool {name}")),
    };
    truncate(result.unwrap_or_else(|e| format!("error: {e}")))
}

fn parse<'a, T: Deserialize<'a>>(arguments: &'a str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

fn bash(cwd: &Path, args: &BashArgs, timeout: Duration) -> Result<String, String> {
    // Its own process group, so a timeout kills the command's children too: killing `sh` alone leaves a
    // grandchild holding the pipes open, and reading their output then blocks until it exits by itself.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&args.command)
        .current_dir(cwd)
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
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
            let _ = child.kill();
            break child.wait().map_err(|e| e.to_string())?;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let out = stdout.take_output();
    let err = stderr.take_output();
    let code = status.code().map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    let killed = if timed_out { format!("killed after {}s\n", timeout.as_secs()) } else { String::new() };
    Ok(format!(
        "{killed}exit code: {code}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    ))
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

fn read_file(cwd: &Path, args: &ReadArgs) -> Result<String, String> {
    fs::read_to_string(cwd.join(&args.path)).map_err(|e| format!("{}: {e}", args.path))
}

fn write_file(cwd: &Path, args: &WriteArgs) -> Result<String, String> {
    let full = cwd.join(&args.path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&full, &args.content).map_err(|e| format!("{}: {e}", args.path))?;
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
        let out = execute(dir.path(), "bash", r#"{"command":"ls; echo oops >&2; exit 3"}"#);
        assert!(out.contains("exit code: 3"), "{out}");
        assert!(out.contains("stdout:\nmarker\n"), "{out}");
        assert!(out.contains("stderr:\noops\n"), "{out}");
    }

    #[test]
    fn bash_is_killed_past_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out =
            execute_with(dir.path(), "bash", r#"{"command":"echo before; sleep 30"}"#, Duration::from_millis(200));
        assert!(out.starts_with("killed after 0s"), "{out}");
        assert!(out.contains("stdout:\nbefore\n"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn write_then_read_relative_path_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let write = json!({"path": "a/b/note.txt", "content": "héllo"}).to_string();
        assert_eq!(execute(dir.path(), "write_file", &write), "wrote 6 bytes to a/b/note.txt");
        assert_eq!(fs::read_to_string(dir.path().join("a/b/note.txt")).unwrap(), "héllo");
        assert_eq!(execute(dir.path(), "read_file", r#"{"path":"a/b/note.txt"}"#), "héllo");
    }

    #[test]
    fn errors_are_returned_not_raised() {
        let root = Path::new("/");
        assert!(execute(root, "read_file", r#"{"path":"/nonexistent/x"}"#).starts_with("error: /nonexistent/x:"));
        assert!(execute(root, "bash", "not json").starts_with("error: invalid arguments:"));
        assert!(execute(root, "bash", "{}").starts_with("error: invalid arguments: missing field `command`"));
        assert!(
            execute(root, "bash", r#"{"command":"ls","path":"x"}"#)
                .starts_with("error: invalid arguments: unknown field `path`, expected `command`")
        );
        assert_eq!(execute(root, "nope", "{}"), "error: unknown tool nope");
    }

    #[test]
    fn truncates_on_char_boundary() {
        let out = truncate("é".repeat(20_000)); // 40 000 bytes, 2 per char
        assert!(out.ends_with("[truncated: first 30000 of 40000 bytes shown]"), "{}", &out[out.len() - 60..]);
    }
}
