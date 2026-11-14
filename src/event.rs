use crate::fleet::{Progress, WorkerEvent, WorkerView};

/// Everything the core asks a frontend to show. The core itself never prints.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UiEvent {
    /// Something the user asked for directly, such as a `/resume` outcome.
    Notice(String),
    /// An overseer turn failed.
    Error(String),
    /// `OVERSEER_DEBUG` output.
    Debug(String),
    /// A new model call began: the transcript starts a new paragraph.
    TurnBoundary,
    /// The overseer's answer, as it streams.
    Delta(String),
    /// The overseer's final answer for one turn.
    Answer(String),
    /// An overseer tool call started; `summary` is what it acts on.
    ToolStarted { id: usize, tool: String, summary: String },
    /// The same call returned, after `secs` and with the text the model reads.
    ToolFinished { id: usize, output: String, secs: f64 },
    /// A worker's reply, final answer or failure.
    Worker { id: usize, kind: WorkerEvent, text: String },
    /// A worker's model output, as it streams.
    WorkerDelta { id: usize, text: String },
    /// A worker tool call, when it starts and when it ends.
    Progress { id: usize, progress: Progress },
    /// The worker roster after a change.
    Workers(Vec<WorkerView>),
    /// An overseer turn started or finished.
    Busy(bool),
    /// The core loop ended; the frontend may quit.
    Done,
}
