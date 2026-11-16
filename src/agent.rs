use crate::compaction::{Compaction, CompactionOutcome, context_tokens, should_compact};
use crate::llm::{Llm, Message, ToolCall};
use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

/// Text of the error that means an overseer order ended the turn. No other path may produce this
/// exact message: a caller matches on it to tell an interrupt from a model or transport failure.
pub(crate) const INTERRUPTED: &str = "interrupted";

/// Tool rounds one turn may run before it gives up. A model that keeps calling tools otherwise
/// loops until an overseer interrupt, growing its history with every round.
const MAX_TOOL_ROUNDS: usize = 200;

/// The model-facing inputs of one turn: the client, the tool schemas it may call, how to run them,
/// the flag an overseer order sets to cut the turn short, and how compaction is wired in.
pub(crate) struct Turn<'a> {
    pub(crate) llm: &'a dyn Llm,
    pub(crate) tool_defs: &'a Value,
    pub(crate) execute: &'a mut dyn FnMut(&ToolCall) -> String,
    pub(crate) cancel: Option<&'a AtomicBool>,
    /// Present when compaction is enabled; the settings decide when it fires.
    pub(crate) compaction: Option<Compaction<'a>>,
    /// Set by `/compact`: the next safe point compacts regardless of the threshold, and the flag
    /// is then cleared so the request fires once.
    pub(crate) force_compact: Option<&'a AtomicBool>,
    /// Called once per attempt, so the caller can log what happened.
    pub(crate) on_compact: Option<&'a mut dyn FnMut(CompactionOutcome)>,
}
impl Turn<'_> {
    /// Runs one turn to completion and returns the model's final text.
    ///
    /// The exchange is committed to `history` only on success: a failed model call leaves the
    /// caller's transcript exactly as it was, so a retry does not replay a half-finished turn.
    pub(crate) fn run(&mut self, history: &mut Vec<Message>, input: &str) -> Result<String> {
        let mut base = history.len();
        history.push(Message::user(input));
        let mut rounds = 0;
        loop {
            if cancelled(self.cancel) {
                history.truncate(base);
                bail!(INTERRUPTED);
            }
            let forced = self.force_compact.is_some_and(|flag| flag.swap(false, Ordering::SeqCst));
            if let Some(compaction) = &self.compaction
                && (forced || should_compact(context_tokens(history), compaction.settings))
            {
                let outcome = compaction.run(history, self.llm, self.cancel, base);
                if let CompactionOutcome::Compacted(report) = &outcome {
                    // The cut never passes `base`, so the current turn's user message keeps its index.
                    base = base + 2 - report.cut;
                }
                if let Some(on_compact) = self.on_compact.as_deref_mut() {
                    on_compact(outcome);
                }
            }
            let reply = match self.llm.chat(history, self.tool_defs, self.cancel) {
                Ok(reply) => reply,
                Err(e) => {
                    history.truncate(base);
                    return Err(e);
                }
            };
            // An order that landed while the call was in flight still owns the outcome: the
            // answer it raced is not the one that was asked for.
            if cancelled(self.cancel) {
                history.truncate(base);
                bail!(INTERRUPTED);
            }
            let text = reply.content.clone().unwrap_or_default();
            let asks = reply.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty());
            // A reply with neither text nor calls is a truncated or filtered answer, not an empty one.
            if !asks && text.trim().is_empty() {
                history.truncate(base);
                bail!("model returned neither text nor tool calls");
            }
            history.push(reply);
            if !asks {
                return Ok(text);
            }
            rounds += 1;
            if rounds > MAX_TOOL_ROUNDS {
                history.truncate(base);
                bail!("model kept calling tools for {MAX_TOOL_ROUNDS} rounds without answering");
            }
            // Read the calls back out of the committed message: the assistant message must keep them,
            // or the provider cannot match the tool results that follow. The calls are copied out
            // first, so cancelling mid-batch can still roll the history back.
            let calls: Vec<ToolCall> =
                history.last().and_then(|message| message.tool_calls.clone()).unwrap_or_default();
            let mut outputs: Vec<(String, String)> = Vec::with_capacity(calls.len());
            for call in &calls {
                if cancelled(self.cancel) {
                    history.truncate(base);
                    bail!(INTERRUPTED);
                }
                outputs.push((call.id.clone(), (self.execute)(call)));
            }
            for (id, output) in outputs {
                history.push(Message::tool(&id, output));
            }
        }
    }
}

/// Whether an overseer order cut this turn short.
pub(crate) fn cancelled(flag: Option<&AtomicBool>) -> bool {
    flag.is_some_and(|flag| flag.load(Ordering::SeqCst))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompactionSettings;
    use crate::llm::testing::{Scripted, call, text};
    use anyhow::bail;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

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
            cancel: None,
            compaction: None,
            force_compact: None,
            on_compact: None,
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
            fn chat(&self, _: &[Message], _: &Value, _: Option<&AtomicBool>) -> Result<Message> {
                bail!("boom")
            }
        }
        let llm = Failing;
        let mut history = vec![Message::system("sys".into())];
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |_: &ToolCall| String::new(),
            cancel: None,
            compaction: None,
            force_compact: None,
            on_compact: None,
        };
        assert!(turn.run(&mut history, "go").is_err());
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "system");
    }

    #[test]
    fn an_empty_reply_is_an_error_and_leaves_history_untouched() {
        let empty = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
            usage: None,
            summary: None,
        };
        let llm = Scripted::new(vec![empty]);
        let mut history = vec![Message::system("sys".into())];
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |_: &ToolCall| String::new(),
            cancel: None,
            compaction: None,
            force_compact: None,
            on_compact: None,
        };
        let err = turn.run(&mut history, "go").unwrap_err();
        assert!(err.to_string().contains("neither text nor tool calls"), "{err}");
        assert_eq!(history.len(), 1, "an empty reply was committed as an answer");
    }

    #[test]
    fn a_cancelled_turn_runs_no_further_tool_call() {
        let flag = AtomicBool::new(false);
        let mut history = Vec::new();
        let mut ran = 0;
        let llm = Scripted::new(vec![
            call("c1", "bash", &json!({"command": "echo one"})),
            call("c2", "bash", &json!({"command": "echo two"})),
            text("done"),
        ]);
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |_: &ToolCall| {
                ran += 1;
                flag.store(true, Ordering::SeqCst);
                "out".to_string()
            },
            cancel: Some(&flag),
            compaction: None,
            force_compact: None,
            on_compact: None,
        };
        let err = turn.run(&mut history, "go").unwrap_err();
        assert_eq!(err.to_string(), "interrupted");
        assert_eq!(ran, 1, "the second call of the batch still ran");
        assert!(history.is_empty(), "an interrupted turn left history behind");
    }

    /// A history two turns deep, so the next turn has something older to compact.
    fn seeded() -> Vec<Message> {
        vec![
            Message::system("sys".into()),
            Message::user("first"),
            text("first answer"),
            Message::user("second"),
            text("second answer"),
        ]
    }

    #[test]
    fn compaction_stays_off_below_the_threshold() {
        let settings = CompactionSettings { threshold_tokens: 10_000, ..Default::default() };
        let llm = Scripted::new(vec![text("answer")]);
        let mut history = seeded();
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |_: &ToolCall| String::new(),
            cancel: None,
            compaction: Some(Compaction { settings: &settings }),
            force_compact: None,
            on_compact: None,
        };
        assert_eq!(turn.run(&mut history, "go").unwrap(), "answer");
        assert!(history.iter().all(|message| message.summary.is_none()));
        assert_eq!(history.len(), 7);
    }

    #[test]
    fn compaction_past_the_threshold_reports_once_and_rebases_the_rollback_point() {
        let settings = CompactionSettings { threshold_tokens: 1, keep_recent_tokens: 1, ..Default::default() };
        let llm = Scripted::new(vec![text("the summary"), text("answer")]);
        let mut history = seeded();
        let mut reports = Vec::new();
        {
            let mut turn = Turn {
                llm: &llm,
                tool_defs: &json!([]),
                execute: &mut |_: &ToolCall| String::new(),
                cancel: None,
                compaction: Some(Compaction { settings: &settings }),
                force_compact: None,
                on_compact: Some(&mut |report| reports.push(report)),
            };
            assert_eq!(turn.run(&mut history, "go").unwrap(), "answer");
        }
        assert_eq!(reports.len(), 1, "one compaction, one report");
        let CompactionOutcome::Compacted(report) = &reports[0] else { panic!("no compaction: {:?}", reports[0]) };
        assert_eq!(report.cut, 4, "the cut stopped at the newest assistant group");
        assert_eq!(history.len(), 5, "system + summary + the kept group + the current turn");
        assert_eq!(history[1].summary.as_deref(), Some("the summary"));
    }

    #[test]
    fn force_compact_fires_once_and_is_consumed() {
        let settings = CompactionSettings { threshold_tokens: 10_000, keep_recent_tokens: 1, ..Default::default() };
        let flag = AtomicBool::new(true);
        let llm = Scripted::new(vec![text("s"), text("answer")]);
        let mut history = seeded();
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |_: &ToolCall| String::new(),
            cancel: None,
            compaction: Some(Compaction { settings: &settings }),
            force_compact: Some(&flag),
            on_compact: None,
        };
        assert_eq!(turn.run(&mut history, "go").unwrap(), "answer");
        assert!(!flag.load(Ordering::SeqCst), "the request fired once and cleared itself");
        assert_eq!(history[1].summary.as_deref(), Some("s"));
    }

    /// A model whose summary call fails and whose answer does not: compaction never fails the turn.
    struct NoSummary;
    impl Llm for NoSummary {
        fn chat(&self, _: &[Message], _: &Value, _: Option<&AtomicBool>) -> Result<Message> {
            Ok(text("answer"))
        }
        fn chat_capped(&self, _: &[Message], _: &Value, _: Option<&AtomicBool>, _: u64) -> Result<Message> {
            bail!("summary failed")
        }
    }

    #[test]
    fn a_failed_compaction_leaves_the_turn_running() {
        let settings = CompactionSettings { threshold_tokens: 1, keep_recent_tokens: 1, ..Default::default() };
        let llm = NoSummary;
        let mut history = seeded();
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |_: &ToolCall| String::new(),
            cancel: None,
            compaction: Some(Compaction { settings: &settings }),
            force_compact: None,
            on_compact: None,
        };
        assert_eq!(turn.run(&mut history, "go").unwrap(), "answer");
        assert!(history.iter().all(|message| message.summary.is_none()), "history stayed uncompacted");
        assert_eq!(history.len(), 7);
    }

    /// Serves one summary, then fails the answer: the rollback runs on a compacted history.
    struct SummaryThenFails(AtomicUsize);
    impl Llm for SummaryThenFails {
        fn chat(&self, _: &[Message], _: &Value, _: Option<&AtomicBool>) -> Result<Message> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(text("the summary"));
            }
            bail!("boom")
        }
    }

    #[test]
    fn a_failure_after_a_compaction_rolls_back_to_a_consistent_history() {
        let settings = CompactionSettings { threshold_tokens: 1, keep_recent_tokens: 1, ..Default::default() };
        let llm = SummaryThenFails(AtomicUsize::new(0));
        let mut history = seeded();
        let mut turn = Turn {
            llm: &llm,
            tool_defs: &json!([]),
            execute: &mut |_: &ToolCall| String::new(),
            cancel: None,
            compaction: Some(Compaction { settings: &settings }),
            force_compact: None,
            on_compact: None,
        };
        let err = turn.run(&mut history, "go").unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert_eq!(history.len(), 3, "the current turn is gone and the summary stays");
        assert_eq!(history[0].role, "system");
        assert_eq!(history[1].summary.as_deref(), Some("the summary"));
        assert!(
            history.iter().all(|message| message.content.as_deref() != Some("go")),
            "the rolled-back turn left nothing behind"
        );
    }
}
