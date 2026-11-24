use crate::{SessionEvent, TurnEndReason};

/// What a session stream says about itself, without replaying every message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Directory the session works in, from `session.created`.
    pub cwd: String,
    /// Model the session was created with, from `session.created`.
    pub model: String,
    /// Turns started so far.
    pub turns: u32,
    /// Reason the last turn ended; `None` before the first one ends.
    pub last_turn: Option<TurnEndReason>,
    /// The last turn started and has no `session.turn_end` yet: running, or interrupted when read
    /// after a restart.
    pub open_turn: bool,
}

impl SessionSummary {
    /// Folds a stream in order; `None` until the `session.created` row.
    #[must_use]
    pub fn fold(events: &[SessionEvent]) -> Option<Self> {
        let mut summary: Option<Self> = None;
        for event in events {
            match event {
                SessionEvent::Created(created) => {
                    summary = Some(Self {
                        cwd: created.cwd.clone(),
                        model: created.model.clone(),
                        turns: 0,
                        last_turn: None,
                        open_turn: false,
                    });
                }
                SessionEvent::UserMessage(_) => {
                    if let Some(summary) = summary.as_mut()
                        && !summary.open_turn
                    {
                        summary.turns += 1;
                        summary.open_turn = true;
                    }
                }
                SessionEvent::TurnEnd(end) => {
                    if let Some(summary) = summary.as_mut() {
                        summary.open_turn = false;
                        summary.last_turn = Some(end.reason);
                    }
                }
                SessionEvent::AssistantMessage(_)
                | SessionEvent::ToolResult(_)
                | SessionEvent::Error(_) => {}
            }
        }
        summary
    }
}
