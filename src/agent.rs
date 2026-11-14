use crate::llm::{Llm, Message, ToolCall};
use anyhow::{Result, bail};
use serde_json::Value;

const MAX_MODEL_CALLS: usize = 50;

/// Runs one turn to completion and returns the model's final text.
pub(crate) fn run_turn(
    llm: &dyn Llm,
    history: &mut Vec<Message>,
    input: &str,
    tool_defs: &Value,
    execute: &mut dyn FnMut(&ToolCall) -> String,
) -> Result<String> {
    history.push(Message::user(input));
    for _ in 0..MAX_MODEL_CALLS {
        let reply = llm.chat(history, tool_defs)?;
        let calls = reply.tool_calls.clone().unwrap_or_default();
        let text = reply.content.clone().unwrap_or_default();
        history.push(reply);
        if calls.is_empty() {
            return Ok(text);
        }
        for call in &calls {
            let output = execute(call);
            history.push(Message::tool(&call.id, output));
        }
    }
    bail!("stopped after {MAX_MODEL_CALLS} model calls without a final answer")
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
        let answer = run_turn(&llm, &mut history, "go", &json!([]), &mut |c: &ToolCall| {
            seen.push(c.function.name.clone());
            "out".to_string()
        })
        .unwrap();
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
        let err =
            run_turn(&Looping, &mut Vec::new(), "loop", &json!([]), &mut |_: &ToolCall| String::new()).unwrap_err();
        assert_eq!(err.to_string(), "stopped after 50 model calls without a final answer");
    }
}
