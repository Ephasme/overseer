use crate::llm::{Llm, Message, ToolCall};
use anyhow::Result;
use serde_json::Value;

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
        let base = history.len();
        history.push(Message::user(input));
        loop {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::testing::{Scripted, call, text};
    use anyhow::bail;
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
        assert_eq!(history[1].tool_calls.as_ref().map(Vec::len), Some(1), "the assistant message lost its call");
        assert_eq!(history[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(history[2].content.as_deref(), Some("out"));
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
