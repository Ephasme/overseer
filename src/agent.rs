use crate::llm::{Llm, Message, ToolCall};
use anyhow::{Result, bail};
use serde_json::Value;

const MAX_MODEL_CALLS: usize = 50;

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
    pub(crate) fn run(&mut self, history: &mut Vec<Message>, input: &str) -> Result<String> {
        let mut pending = vec![Message::user(input)];
        for _ in 0..MAX_MODEL_CALLS {
            let mut seen = history.clone();
            seen.extend_from_slice(&pending);
            let reply = self.llm.chat(&seen, self.tool_defs)?;
            let calls = reply.tool_calls.clone().unwrap_or_default();
            let text = reply.content.clone().unwrap_or_default();
            pending.push(reply);
            if calls.is_empty() {
                history.append(&mut pending);
                return Ok(text);
            }
            for call in &calls {
                let output = (self.execute)(call);
                pending.push(Message::tool(&call.id, output));
            }
        }
        bail!("stopped after {MAX_MODEL_CALLS} model calls without a final answer")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::testing::{Scripted, call, text};
    use serde_json::json;

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
