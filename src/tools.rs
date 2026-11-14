use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

const MAX_OUTPUT_BYTES: usize = 30_000;

/// Schemas of the file and shell tools every worker gets.
pub(crate) fn definitions() -> Value {
    json!([
        {"type": "function", "function": {
            "name": "bash",
            "description": "Run a shell command with `sh -c` in the repository directory. Returns exit code, stdout and stderr. Stdin is closed.",
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

/// Runs one tool call inside `cwd`; the returned string goes back to the model as the tool result.
pub(crate) fn execute(cwd: &Path, name: &str, arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(e) => return format!("error: invalid arguments: {e}"),
    };
    let result = match name {
        "bash" => bash(cwd, &args),
        "read_file" => read_file(cwd, &args),
        "write_file" => write_file(cwd, &args),
        _ => Err(format!("unknown tool {name}")),
    };
    truncate(result.unwrap_or_else(|e| format!("error: {e}")))
}

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string argument '{key}'"))
}

fn bash(cwd: &Path, args: &Value) -> Result<String, String> {
    let command = str_arg(args, "command")?;
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let code = out.status.code().map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    Ok(format!(
        "exit code: {code}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

fn read_file(cwd: &Path, args: &Value) -> Result<String, String> {
    let path = str_arg(args, "path")?;
    fs::read_to_string(cwd.join(path)).map_err(|e| format!("{path}: {e}"))
}

fn write_file(cwd: &Path, args: &Value) -> Result<String, String> {
    let path = str_arg(args, "path")?;
    let content = str_arg(args, "content")?;
    let full = cwd.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&full, content).map_err(|e| format!("{path}: {e}"))?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
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
        assert_eq!(execute(root, "bash", "{}"), "error: missing string argument 'command'");
        assert_eq!(execute(root, "nope", "{}"), "error: unknown tool nope");
    }

    #[test]
    fn truncates_on_char_boundary() {
        let out = truncate("é".repeat(20_000)); // 40 000 bytes, 2 per char
        assert!(out.ends_with("[truncated: first 30000 of 40000 bytes shown]"), "{}", &out[out.len() - 60..]);
    }
}
