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
    /// When set, a path argument that resolves outside `cwd` is refused instead of honoured.
    confined: bool,
}

impl ToolRegistry {
    /// Rooted at `cwd`, which should be absolute: relative paths are resolved against it.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            confined: false,
        }
    }

    /// Refuses every path argument that resolves outside the session directory.
    #[must_use]
    pub fn confined(mut self) -> Self {
        self.confined = true;
        self
    }

    /// `Some(message)` when a path argument escapes a confined root.
    fn scope_check(&self, path: Option<&str>) -> Option<String> {
        if !self.confined {
            return None;
        }
        let path = path?;
        let root = normalize(&self.cwd);
        if normalize(&self.cwd.join(path)).starts_with(&root) {
            None
        } else {
            Some(format!("path {path:?} is outside the session directory"))
        }
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
    /// kills a running command's process group. Never returns an error: a failure is a
    /// `ToolOutcome` with `ok` false.
    ///
    /// Cancel through `cancel`, not by dropping this future: dropping kills the command's process
    /// group too, but no result is recorded for the call.
    pub async fn call(
        &self,
        name: &str,
        arguments: &str,
        progress: &mut (dyn FnMut(&str) + Send),
        cancel: &CancellationToken,
    ) -> ToolOutcome {
        match name {
            "bash" => match parse::<BashArgs>(arguments) {
                Ok(args) => match self.scope_check(args.workdir.as_deref()) {
                    Some(message) => ToolOutcome::error(message),
                    None => bash::run(&self.cwd, args, progress, cancel).await,
                },
                Err(message) => ToolOutcome::error(message),
            },
            "read_file" => match parse::<ReadArgs>(arguments) {
                Ok(args) => match self.scope_check(Some(&args.path)) {
                    Some(message) => ToolOutcome::error(message),
                    None => files::read(&self.cwd, args).await,
                },
                Err(message) => ToolOutcome::error(message),
            },
            "write_file" => match parse::<WriteArgs>(arguments) {
                Ok(args) => match self.scope_check(Some(&args.path)) {
                    Some(message) => ToolOutcome::error(message),
                    None => files::write(&self.cwd, args).await,
                },
                Err(message) => ToolOutcome::error(message),
            },
            "edit_file" => match parse::<EditArgs>(arguments) {
                Ok(args) => match self.scope_check(Some(&args.path)) {
                    Some(message) => ToolOutcome::error(message),
                    None => files::edit(&self.cwd, args).await,
                },
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

/// Lexical normalization: resolves `.` and `..` without touching the filesystem, so a confined
/// check cannot be defeated by a path that does not exist yet.
fn normalize(path: &std::path::Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
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
