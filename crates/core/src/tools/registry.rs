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
