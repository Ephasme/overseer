//! The tools the model may call, and the registry that runs them.
//!
//! Every failure — unknown tool, arguments that do not match the schema, a missing file, a command
//! that exits non-zero — is returned to the model as a result with `ok: false`, never as an error
//! out of [`ToolRegistry::call`].

mod bash;
mod cap;
mod files;
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
