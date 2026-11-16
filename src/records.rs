//! Machine-readable session records: one JSONL file per agent, written as turns run.
//!
//! The plaintext log (`agent-<id>.log`) was written but never read back; these records are the
//! substrate resume (SPC-310), search and export (SPC-385) build on. Every message is recorded
//! exactly as the model saw it: a `bash` command or a `write_file` body lives verbatim in the
//! assistant record's tool-call arguments.
//!
//! Replay contract (for SPC-310, not implemented here): a turn replays only when its
//! `Outcome` kind is `Finished`. `Interrupted` and `Failed` turns truncate the in-memory
//! history in `Turn::run`, so their records are kept for forensics but skipped on replay.
//! The `Assistant` and `Tool` records between an `Instruction` and its `Outcome` belong to
//! that turn — including rounds a later failure truncated from memory.
//!
//! Writes are append-only and best effort: a serialization or io error is dropped, exactly
//! like the plaintext log's `writeln!` calls, so a full disk must not kill the agent. A
//! crashed write may leave a partial last line; the renderer skips it.

use crate::llm;
use crate::swarm::describe;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// One committed message of one agent's session. `ts` (unix milliseconds) is metadata for
/// export and search, never a sort key: records are ordered by write order within an agent.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Record {
    /// An instruction the owner sent the agent (or the close report the owner received).
    Instruction { text: String },
    /// A committed assistant reply, exactly as the model produced it (tool calls included).
    Assistant { message: llm::Message },
    /// One committed tool result; `call_id` matches the assistant message's call id.
    Tool { call_id: String, output: String },
    /// How the turn ended. `Closed` = the agent's own `close` tool; its text is always empty
    /// (the close report reaches the owner as an `Instruction` record in the owner's file).
    Outcome { outcome: Outcome, text: String },
}

/// How a turn ended, spelled the way the plaintext log spelled agent outcomes.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Finished,
    Failed,
    Interrupted,
    Closed,
}
impl Outcome {
    /// The label the rendered log shows. `Closed` renders without text (see `render`).
    fn as_label(self) -> &'static str {
        match self {
            Outcome::Finished => "Finished",
            Outcome::Failed => "Failed",
            Outcome::Interrupted => "Interrupted",
            Outcome::Closed => "Closed",
        }
    }
}
/// One record line: `ts` plus the tagged record. `Record` carries the payload only —
/// `write` fills `ts` itself, so callers never fabricate one. Built with `to_value` and a
/// manual `ts` insert rather than a wrapper struct: a `#[serde(flatten)]` of an internally
/// tagged enum swallows the tag instead of merging it, which the parser would refuse.
fn line(ts: u64, record: &Record) -> Option<String> {
    let mut value = serde_json::to_value(record).ok()?;
    value.as_object_mut()?.insert("ts".to_string(), serde_json::Value::from(ts));
    let mut text = serde_json::to_string(&value).ok()?;
    text.push('\n');
    Some(text)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// The append-only JSONL file of one agent. Shared behind `&self`: both the execute closure
/// and the commit-hook closure of a turn capture the same reference.
pub(crate) struct Writer {
    file: Mutex<File>,
}

impl Writer {
    pub(crate) fn create(path: &Path) -> std::io::Result<Writer> {
        // The records hold full-fidelity payloads (commands, file bodies, raw output): they
        // stay private to the session's user, whatever the process umask is.
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        Ok(Writer { file: Mutex::new(file) })
    }

    /// Appends one record on its own line. Best effort: an error is dropped.
    pub(crate) fn write(&self, record: &Record) {
        if let Some(text) = line(now(), record)
            && let Ok(mut file) = self.file.lock()
        {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

/// Renders records back into the plaintext log shape the agents and tests read:
/// `> instruction`, `· tool (args bytes) summary`, the raw output, `= Outcome: text`.
///
/// The caller bounds memory: `history://` reads the file through `tools::read_limited`, so
/// this never sees more than the read budget. Unparsable lines (a crashed partial write, an
/// unknown kind) are skipped silently. A `Tool` record with no matching assistant call still
/// renders as its bare output, whatever a future format change does.
pub(crate) fn render(text: &str) -> String {
    let records: Vec<Record> = text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    let mut consumed = vec![false; records.len()];
    let mut out: Vec<String> = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let candidate = match record {
            Record::Instruction { text } => format!("> {text}"),
            Record::Assistant { message } => {
                for call in message.tool_calls.iter().flatten() {
                    // Matched by call id, not order alone: a batch's results land in call
                    // order, but only the id pairing is guaranteed.
                    let matched = records.iter().enumerate().skip(i + 1).find(|(j, r)| match r {
                        Record::Tool { call_id, .. } if call_id == &call.id => {
                            // `enumerate` precedes `skip`, so `j` is already absolute.
                            !consumed.get(*j).copied().unwrap_or(false)
                        }
                        _ => false,
                    });
                    if let Some((j, Record::Tool { output, .. })) = matched {
                        if let Some(slot) = consumed.get_mut(j) {
                            *slot = true;
                        }
                        // The legacy log printed the `·` line just before execution, so a
                        // call with no result (a `close`, a batch cut short) never had one.
                        let summary = describe(&call.function.name, &call.function.arguments);
                        out.push(format!(
                            "· {} ({} bytes) {summary}",
                            call.function.name,
                            call.function.arguments.len()
                        ));
                        out.push(output.clone());
                    }
                }
                continue;
            }
            // An unpaired tool result still shows, whatever a future format change does.
            Record::Tool { output, .. } if !consumed.get(i).copied().unwrap_or(false) => output.clone(),
            Record::Tool { .. } => continue,
            Record::Outcome { outcome: Outcome::Closed, .. } => "= Closed".to_string(),
            Record::Outcome { outcome, text } => format!("= {}: {text}", outcome.as_label()),
        };
        out.push(candidate);
    }
    if out.is_empty() { String::new() } else { format!("{}\n", out.join("\n")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{FunctionCall, Message, ToolCall};
    use serde_json::json;

    /// One scripted turn: instruction, assistant with one tool call, tool result, outcome.
    fn scripted_turn() -> Vec<Record> {
        vec![
            Record::Instruction { text: "go".to_string() },
            Record::Assistant {
                message: Message {
                    role: "assistant".into(),
                    content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "c1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: json!({"command": "echo hi"}).to_string(),
                        },
                    }]),
                    tool_call_id: None,
                },
            },
            Record::Tool { call_id: "c1".into(), output: "hi\n".to_string() },
            Record::Outcome { outcome: Outcome::Finished, text: "done".to_string() },
        ]
    }

    fn write_all(records: &[Record]) -> String {
        let text: String = records
            .iter()
            .map(|record| {
                let mut line = line(now(), record).unwrap();
                line.push('\n');
                line
            })
            .collect();
        render(&text)
    }

    #[test]
    fn a_scripted_turn_renders_the_legacy_lines() {
        let rendered = write_all(&scripted_turn());
        let mut lines = rendered.lines();
        assert_eq!(lines.next(), Some("> go"));
        assert_eq!(lines.next(), Some("· bash (21 bytes) echo hi"));
        assert_eq!(lines.next(), Some("hi"));
        // The legacy log wrote the output with `writeln!`, so a trailing newline in the
        // output reads back as one blank line; render reproduces that exactly.
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), Some("= Finished: done"));
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn tool_records_pair_by_call_id_and_strays_render_bare() {
        let mut records = scripted_turn();
        // A second batch whose result carries an id no assistant named: renders as bare output.
        records.insert(3, Record::Tool { call_id: "lost".into(), output: "stray\n".to_string() });
        let rendered = write_all(&records);
        assert!(rendered.contains("stray\n"), "{rendered}");
        assert!(rendered.contains("hi\n"), "{rendered}");
        assert!(rendered.contains("= Finished: done"), "{rendered}");
    }

    #[test]
    fn an_unknown_kind_and_a_partial_line_are_skipped() {
        let mut text = String::new();
        for record in scripted_turn() {
            let mut line = line(now(), &record).unwrap();
            line.push('\n');
            text.push_str(&line);
        }
        text.push_str("{\"kind\":\"surprise\"}\n");
        text.push_str("{\"kind\":\"instruction\",\"ts\":1,\"tex"); // a crashed write's partial line
        assert!(render(&text).contains("> go"));
        assert!(render(&text).contains("= Finished: done"));
        assert!(!render(&text).contains("surprise"), "{}", render(&text));
    }

    #[test]
    fn an_outcome_renders_closed_without_text() {
        let rendered = write_all(&[Record::Outcome { outcome: Outcome::Closed, text: String::new() }]);
        assert_eq!(rendered, "= Closed\n");
        let rendered = write_all(&[Record::Outcome { outcome: Outcome::Interrupted, text: "cut".into() }]);
        assert_eq!(rendered, "= Interrupted: cut\n");
    }

    #[test]
    fn a_written_line_round_trips() {
        let writer_dir = tempfile::tempdir().unwrap();
        let writer = Writer::create(&writer_dir.path().join("agent-1.jsonl")).unwrap();
        for record in scripted_turn() {
            writer.write(&record);
        }
        let text = std::fs::read_to_string(writer_dir.path().join("agent-1.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 4);
        for line in text.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(value.get("ts").is_some_and(serde_json::Value::is_u64), "{line}");
            assert!(value.get("kind").is_some(), "{line}");
        }
        // Rendered, the file reads back as the legacy lines.
        assert!(render(&text).starts_with("> go\n"));
        // The wire shape of the assistant message survives byte-for-byte.
        let assistant: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(
            assistant["message"]["tool_calls"][0]["function"]["arguments"],
            json!({"command": "echo hi"}).to_string()
        );
    }
}
