use crate::swarm::{AgentEvent, AgentView, Progress};

/// Everything the core asks a frontend to show. The core itself never prints.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UiEvent {
    /// Something the user asked for directly, such as a `/resume` outcome.
    Notice(String),
    /// A failure the user must see: an agent turn, a model call, or the core failing to start.
    Error(String),
    /// `OVERSEER_DEBUG` output.
    Debug(String),
    /// An agent began a new model call: its stream starts a new paragraph.
    Boundary { id: usize },
    /// The instruction an agent is about to work on.
    Instruction { id: usize, text: String },
    /// The root's final answer for one turn.
    Answer(String),
    /// An agent's reply, final answer or failure.
    Agent { id: usize, kind: AgentEvent, text: String },
    /// An agent's model output, as it streams.
    AgentDelta { id: usize, text: String },
    /// An agent's model reasoning, as it streams. Shown, never fed back.
    AgentThought { id: usize, text: String },
    /// A tool call, when it starts and when it ends.
    Progress { id: usize, progress: Progress },
    /// The tree after a change.
    Agents(Vec<AgentView>),
    /// An agent turn started or finished.
    Busy(bool),
    /// The core loop ended; the frontend may quit.
    Done,
}
