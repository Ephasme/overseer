use crate::llm::{Llm, Message, ToolCall};
use anyhow::{Result, bail};
use serde_json::Value;

const MAX_MODEL_CALLS: usize = 50;

/// Roughly a 100k-token conversation: older turns are dropped before the next call rather than
/// letting the session reach a context-length error it could never recover from.
const HISTORY_BUDGET: usize = 400_000;

/// What one message costs on the wire, near enough: contents plus tool names and arguments.
fn message_size(message: &Message) -> usize {
    let content = message.content.as_ref().map_or(0, String::len);
    let calls = message.tool_calls.as_ref().map_or(0, |calls| {
        calls.iter().map(|call| call.id.len() + call.function.name.len() + call.function.arguments.len()).sum()
    });
    content + calls + 16
}

/// Drops the oldest turns until `history` fits the budget, keeping the system prompt and the newest
/// turn whole: a provider rejects a `tool` message whose `assistant` message was dropped.
fn trim(history: &mut Vec<Message>) {
    let sizes: Vec<usize> = history.iter().map(message_size).collect();
    let mut kept = 0;
    let mut end = history.len();
    while end > 1 {
        let next = end - 1;
        let candidate = kept + sizes.get(next).copied().unwrap_or_default();
        if candidate > HISTORY_BUDGET {
            break;
        }
        kept = candidate;
        end = next;
    }
    while end < history.len() && history.get(end).is_none_or(|message| message.role != "user") {
        end += 1;
    }
    if end > 1 && end < history.len() {
        history.drain(1..end);
    }
}

/// The model-facing inputs of one turn: the client, the tool schemas it may call, and how to run them.
pub(crate) struct Turn<'a> {
    pub(crate) llm: &'a dyn Llm,
    pub(crate) tool_defs: &'a Value,
    pub(crate) execute: &'a mut dyn FnMut(&ToolCall) -> String,
}

impl Turn<'_> {
    /// Runs one turn to completion and returns the model's final text.
    ///
    /// The exchange is committed to `history` only on success: a failed model call leaves the
    /// caller's transcript exactly as it was, so a retry does not replay a half-finished turn.
    /// Turns already over `HISTORY_BUDGET` are dropped first, and stay dropped.
    pub(crate) fn run(&mut self, history: &mut Vec<Message>, input: &str) -> Result<String> {
        trim(history);
        let base = history.len();
        history.push(Message::user(input));
        for _ in 0..MAX_MODEL_CALLS {
            let reply = match self.llm.chat(history, self.tool_defs) {
                Ok(reply) => reply,
                Err(e) => {
                    history.truncate(base);
                    return Err(e);
                }
            };
            let text = reply.content.clone().unwrap_or_default();
            let asks = reply.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty());
            history.push(reply);
            if !asks {
                return Ok(text);
            }
            // Read the calls back out of the committed message: the assistant message must keep them,
            // or the provider cannot match the tool results that follow.
            let outputs: Vec<(String, String)> = history
                .last()
                .and_then(|message| message.tool_calls.as_deref())
                .unwrap_or_default()
                .iter()
                .map(|call| (call.id.clone(), (self.execute)(call)))
                .collect();
            for (id, output) in outputs {
                history.push(Message::tool(&id, output));
            }
        }
        history.truncate(base);
        bail!("stopped after {MAX_MODEL_CALLS} model calls without a final answer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::testing::{Scripted, call, text};
    use serde_json::json;

    fn sized_history(turns: usize) -> Vec<Message> {
        let mut history = vec![Message::system("sys".into())];
        for turn in 0..turns {
            history.push(Message::user(&format!("{turn}{}", "u".repeat(90_000))));
            history.push(text(&format!("{turn}{}", "a".repeat(90_000))));
        }
        history
    }

    #[test]
    fn trim_drops_the_oldest_turns_and_starts_on_a_user_message() {
        let mut history = sized_history(6);
        let last = history.last().cloned();
        trim(&mut history);
        assert_eq!(history.first().map(|m| m.role.as_str()), Some("system"));
        assert_eq!(history.get(1).map(|m| m.role.as_str()), Some("user"), "a tool result lost its assistant message");
        assert_eq!(history.last(), last.as_ref(), "the newest turn is never dropped");
        let total: usize = history.iter().map(message_size).sum();
        assert!(total <= HISTORY_BUDGET, "{total} still over budget");
    }

    #[test]
    fn trim_never_empties_one_oversized_turn() {
        let mut history = vec![Message::system("sys".into())];
        history.push(Message::user(&"u".repeat(HISTORY_BUDGET * 2)));
        trim(&mut history);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn trim_leaves_a_conversation_under_budget_alone() {
        let mut history = sized_history(2);
        let before = history.clone();
        trim(&mut history);
        assert_eq!(history, before);
    }

    #[test]
    fn executes_tool_calls_until_text_reply() {
        let llm = Scripted::new(vec![call("c1", "bash", &json!({"command": "echo hi"})), text("done")]);
        let mut history = Vec::new();
        let mut seen = Vec::new();
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |c: &ToolCall| {
                seen.push(c.function.name.clone());
                "out".to_string()
            },
        };
        let answer = turn.run(&mut history, "go").unwrap();
        assert_eq!(answer, "done");
        assert_eq!(seen, ["bash"]);
        let roles: Vec<&str> = history.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
        assert_eq!(history[1].tool_calls.as_ref().map(Vec::len), Some(1), "the assistant message lost its call");
        assert_eq!(history[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(history[2].content.as_deref(), Some("out"));
    }

    #[test]
    fn gives_up_after_cap() {
        struct Looping;
        impl Llm for Looping {
            fn chat(&self, _: &[Message], _: &Value) -> Result<Message> {
                Ok(call("c", "bash", &json!({})))
            }
        }
        let llm = Looping;
        let mut turn = Turn { llm: &llm, tool_defs: &json!([]), execute: &mut |_: &ToolCall| String::new() };
        let err = turn.run(&mut Vec::new(), "loop").unwrap_err();
        assert_eq!(err.to_string(), "stopped after 50 model calls without a final answer");
    }

    #[test]
    fn failed_chat_leaves_history_untouched() {
        struct Failing;
        impl Llm for Failing {
            fn chat(&self, _: &[Message], _: &Value) -> Result<Message> {
                bail!("boom")
            }
        }
        let llm = Failing;
        let mut history = vec![Message::system("sys".into())];
        let mut turn = Turn { llm: &llm, tool_defs: &json!([]), execute: &mut |_: &ToolCall| String::new() };
        assert!(turn.run(&mut history, "go").is_err());
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "system");
    }
}
