use serde::Deserialize;
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
    let result = match name {
        "bash" => parse::<BashArgs>(arguments).and_then(|args| bash(cwd, &args)),
        "read_file" => parse::<ReadArgs>(arguments).and_then(|args| read_file(cwd, &args)),
        "write_file" => parse::<WriteArgs>(arguments).and_then(|args| write_file(cwd, &args)),
        _ => Err(format!("unknown tool {name}")),
    };
    truncate(result.unwrap_or_else(|e| format!("error: {e}")))
}

fn parse<'a, T: Deserialize<'a>>(arguments: &'a str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

fn bash(cwd: &Path, args: &BashArgs) -> Result<String, String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(&args.command)
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
