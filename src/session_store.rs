//! Durable session state: one `session.jsonl` (append-only tree records) and one
//! `agent-<id>.history.json` per agent, under the session dir. Everything the resume path needs
//! reads or writes through this module; nothing else touches the files.

use crate::llm::Message;
use crate::{config, profiles, tools};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

/// One record of the tree, tagged by `kind`. Appended as the session runs; replayed on resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum TreeRecord {
    /// The session header: when it started, where it lives, which session it resumes.
    Session {
        at: u64,
        dir: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumed_from: Option<String>,
    },
    /// One agent entering the tree, live or already dead.
    Agent {
        id: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<usize>,
        depth: usize,
        dir: String,
        task: String,
        profile: String,
        role: profiles::Role,
        policy: tools::Policy,
        stopped: bool,
        archived: bool,
        pipeline: bool,
    },
    /// An agent's display name changed.
    Rename { id: usize, name: String },
    /// A transition to dead: `stop` or archive. Only ever to stopped/archived, never back.
    State { id: usize, stopped: bool, archived: bool },
    /// First user instruction of the root's first successful turn, capped at 100 chars.
    Title { text: String },
}

/// Which previous session a run resumes.
#[derive(Debug, Clone)]
pub(crate) struct ResumeSource {
    pub(crate) id: String,
    pub(crate) dir: PathBuf,
}

/// The append-only `session.jsonl` of one session dir. Every write flushes and syncs, so a
/// `kill -9` keeps what landed.
pub(crate) struct SessionLog {
    file: File,
}
impl SessionLog {
    pub(crate) fn open(dir: &Path) -> Result<Self, String> {
        // The caller creates the session dir: a missing one is a startup failure, not a silent fix.
        let path = dir.join("session.jsonl");
        let file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|e| config::io_error(path.display(), &e))?;
        Ok(Self { file })
    }

    /// Append several lines in one write and one sync, so a torn line can only ever be the last
    /// one. Best-effort by design: a persistence failure never takes the session down, and it never
    /// prints either, because the core thread shares the terminal with the TUI. The resume falls
    /// back to the last record that landed, the same rule as the text logs.
    fn record_many(&mut self, lines: &[&str]) {
        let mut buf = String::new();
        for line in lines {
            buf.push_str(line);
            buf.push('\n');
        }
        if self.file.write_all(buf.as_bytes()).is_err() {
            return;
        }
        let _ = self.file.flush();
        let _ = self.file.sync_all();
    }

    /// Serialize one record and append it.
    pub(crate) fn write(&mut self, record: &TreeRecord) {
        self.write_many(std::slice::from_ref(record));
    }

    /// Serialize several records and append them in one write: `stop` writes one per descendant,
    /// and a sync per record would hold the tree lock for each.
    pub(crate) fn write_many(&mut self, records: &[TreeRecord]) {
        let lines: Vec<String> = records.iter().filter_map(|record| serde_json::to_string(record).ok()).collect();
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        self.record_many(&borrowed);
    }
}

/// One resumable session, as the picker and `-c` see it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) dir: PathBuf,
    pub(crate) title: Option<String>,
}

/// Every resumable session under `state/sessions`, newest first. A dir that holds only the
/// `Session` header (a crash between the header and the first agent) or a corrupt log is not
/// resumable and never listed, so `-c` cannot pick one it would fail on.
pub(crate) fn sessions(state: &Path) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for (dir, id) in session_dirs(state) {
        let Ok(records) = read_tree(&dir) else { continue };
        if !has_agents(&records) {
            continue;
        }
        let title = records.iter().find_map(|record| match record {
            TreeRecord::Title { text } => Some(text.clone()),
            _ => None,
        });
        out.push(SessionInfo { id, dir, title });
    }
    out
}

/// The newest resumable session, without parsing the older logs: `-c` only needs one.
pub(crate) fn newest(state: &Path) -> Option<SessionInfo> {
    for (dir, id) in session_dirs(state) {
        let Ok(records) = read_tree(&dir) else { continue };
        if !has_agents(&records) {
            continue;
        }
        let title = records.iter().find_map(|record| match record {
            TreeRecord::Title { text } => Some(text.clone()),
            _ => None,
        });
        return Some(SessionInfo { id, dir, title });
    }
    None
}

/// Whether a session dir holds an agent to resume. A `session.jsonl` alone is not enough: the
/// header lands before the first agent record.
pub(crate) fn resumable(dir: &Path) -> bool {
    read_tree(dir).is_ok_and(|records| has_agents(&records))
}

fn has_agents(records: &[TreeRecord]) -> bool {
    records.iter().any(|record| matches!(record, TreeRecord::Agent { .. }))
}

/// Every dir under `state/sessions` that holds a `session.jsonl`, newest first. The id embeds the
/// start timestamp, so reverse name order is newest first.
fn session_dirs(state: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = fs::read_dir(state.join("sessions")) else {
        return Vec::new();
    };
    let mut dirs: Vec<(PathBuf, String)> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| (e.path(), e.file_name().to_string_lossy().into_owned()))
        .filter(|(p, _)| p.join("session.jsonl").is_file())
        .collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1));
    dirs
}

/// Parse a session's `session.jsonl`. A missing file is an error: a session dir without one is
/// not resumable. A corrupt file is an error naming it.
pub(crate) fn read_tree(dir: &Path) -> Result<Vec<TreeRecord>, String> {
    let path = dir.join("session.jsonl");
    let text = fs::read_to_string(&path).map_err(|e| config::io_error(path.display(), &e))?;
    // A crash mid-append leaves a torn last line with no newline after it: drop that one and keep
    // everything before it. A bad line anywhere else is still an error.
    let unterminated = !text.is_empty() && !text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(record) => out.push(record),
            Err(_) if unterminated && n + 1 == lines.len() => break,
            Err(e) => return Err(format!("{}: line {}: not a session record: {e}", path.display(), n + 1)),
        }
    }
    Ok(out)
}

/// One agent of a session, with the `State` and `Rename` records folded in: what a resume builds
/// a row from. `spawn` writes `stopped: false`, so without the fold a stopped agent would come back
/// live and rerun work the user killed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentRow {
    pub(crate) id: usize,
    pub(crate) name: Option<String>,
    pub(crate) parent: Option<usize>,
    pub(crate) depth: usize,
    pub(crate) dir: String,
    pub(crate) task: String,
    pub(crate) profile: String,
    pub(crate) role: profiles::Role,
    pub(crate) policy: tools::Policy,
    pub(crate) stopped: bool,
    pub(crate) archived: bool,
    pub(crate) pipeline: bool,
}

/// The session's agents, in id order, with `State` and `Rename` applied in log order. Ids index
/// the tree (`id - 1`), so a gap or a record for an id that was never introduced is refused rather
/// than silently misindexing every lookup.
pub(crate) fn rows(dir: &Path) -> Result<Vec<AgentRow>, String> {
    let path = dir.join("session.jsonl");
    let mut rows: Vec<AgentRow> = Vec::new();
    for record in read_tree(dir)? {
        match record {
            TreeRecord::Agent {
                id,
                name,
                parent,
                depth,
                dir,
                task,
                profile,
                role,
                policy,
                stopped,
                archived,
                pipeline,
            } => {
                if id != rows.len() + 1 {
                    return Err(format!("{}: agent {id} is out of order", path.display()));
                }
                rows.push(AgentRow {
                    id,
                    name,
                    parent,
                    depth,
                    dir,
                    task,
                    profile,
                    role,
                    policy,
                    stopped,
                    archived,
                    pipeline,
                });
            }
            TreeRecord::State { id, stopped, archived } => {
                let row = rows
                    .get_mut(id.saturating_sub(1))
                    .filter(|row| row.id == id)
                    .ok_or_else(|| format!("{}: state for unknown agent {id}", path.display()))?;
                row.stopped = stopped;
                row.archived = archived;
            }
            TreeRecord::Rename { id, name } => {
                let row = rows
                    .get_mut(id.saturating_sub(1))
                    .filter(|row| row.id == id)
                    .ok_or_else(|| format!("{}: rename for unknown agent {id}", path.display()))?;
                row.name = (!name.is_empty()).then_some(name);
            }
            TreeRecord::Session { .. } | TreeRecord::Title { .. } => {}
        }
    }
    Ok(rows)
}

/// Load one agent's committed history. A missing file is a fresh history (the agent never
/// completed a turn); an unparsable one is an error naming the file.
pub(crate) fn load_history(path: &Path) -> Result<Vec<Message>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(config::io_error(path.display(), &e)),
    };
    serde_json::from_str(&text).map_err(|e| format!("{}: not an agent history: {e}", path.display()))
}

/// Store one agent's committed history: temp file then rename, so a reader never sees half. Same
/// pattern as `memory::write`.
pub(crate) fn save_history(path: &Path, history: &[Message]) -> Result<(), String> {
    let text = serde_json::to_string(history).map_err(|e| format!("{}: serialize: {e}", path.display()))?;
    let temp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    // Synced before the rename: a crash must not leave an empty history where the tree record
    // already says the turn committed.
    let write = fs::File::create(&temp).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    if let Err(e) = write {
        let _ = fs::remove_file(&temp);
        return Err(config::io_error(temp.display(), &e));
    }
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(config::io_error(path.display(), &e));
    }
    Ok(())
}

/// The picker preview line for one session row. The title is the user's own first instruction, so
/// control characters are stripped before it reaches the terminal.
pub(crate) fn picker_row(info: &SessionInfo, now: u64) -> String {
    let title = info.title.clone().unwrap_or_else(|| info.dir.display().to_string());
    let age = now.checked_sub(session_start(info)).map_or_else(|| "now".to_string(), secs_ago);
    format!("{}  {}  {}", printable(&info.id), printable(&title), age)
}

/// `text` with every control character dropped: a session title must not move the cursor.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn session_start(info: &SessionInfo) -> u64 {
    info.id.split('-').next().and_then(|t| t.parse().ok()).unwrap_or(0)
}

/// Human seconds ago, one granularity only: the picker row stays short.
fn secs_ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn policy() -> tools::Policy {
        tools::Policy { allow_tools: vec!["bash".into(), "read_file".into()], read_only: true, ..Default::default() }
    }

    fn agent_record(id: usize) -> TreeRecord {
        TreeRecord::Agent {
            id,
            name: Some("harbor falcon 7".into()),
            parent: if id == 1 { None } else { Some(1) },
            depth: id.saturating_sub(1),
            dir: "/tmp".into(),
            task: "do the thing".into(),
            profile: "worker".into(),
            role: profiles::Role::Worker,
            policy: policy(),
            stopped: false,
            archived: false,
            pipeline: false,
        }
    }

    #[test]
    fn a_history_round_trips() {
        let dir = temp_dir();
        let path = dir.path().join("agent-1.history.json");
        let history = vec![
            llm::Message::system("you are a supervisor".into()),
            llm::Message::user("list the files"),
            llm::Message {
                usage: None,
                summary: None,
                role: "assistant".into(),
                content: None,
                tool_calls: Some(vec![llm::ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: llm::FunctionCall { name: "bash".into(), arguments: r#"{"command":"ls"}"#.into() },
                }]),
                tool_call_id: None,
            },
            llm::Message::tool("call_1", "a.txt\nb.txt".into()),
            llm::Message::user("thanks"),
        ];
        save_history(&path, &history).unwrap();
        let loaded = load_history(&path).unwrap();
        assert_eq!(loaded, history);
    }

    #[test]
    fn a_missing_history_is_an_empty_one() {
        let dir = temp_dir();
        let empty = load_history(&dir.path().join("agent-9.history.json")).unwrap();
        assert!(empty.is_empty(), "{empty:?}");
    }

    #[test]
    fn a_corrupt_history_names_the_file() {
        let dir = temp_dir();
        let path = dir.path().join("agent-1.history.json");
        fs::write(&path, "{not json").unwrap();
        let err = load_history(&path).unwrap_err();
        assert!(err.contains("agent-1.history.json"), "{err}");
    }

    #[test]
    fn tree_records_replay_identically() {
        let dir = temp_dir();
        let mut log = SessionLog::open(dir.path()).unwrap();
        log.write(&TreeRecord::Session { at: 42, dir: "/tmp/s1".into(), resumed_from: None });
        log.write(&agent_record(1));
        log.write(&agent_record(2));
        log.write(&TreeRecord::Rename { id: 2, name: "calm cloud 1".into() });
        log.write(&TreeRecord::State { id: 2, stopped: true, archived: false });
        log.write(&TreeRecord::Title { text: "fix the bug".into() });
        drop(log);

        let records = read_tree(dir.path()).unwrap();
        assert_eq!(records.len(), 6);
        assert_eq!(records[0], TreeRecord::Session { at: 42, dir: "/tmp/s1".into(), resumed_from: None });
        assert_eq!(records[1], agent_record(1));
        assert_eq!(records[3], TreeRecord::Rename { id: 2, name: "calm cloud 1".into() });
        assert_eq!(records[4], TreeRecord::State { id: 2, stopped: true, archived: false });
        assert_eq!(records[5], TreeRecord::Title { text: "fix the bug".into() });
    }

    #[test]
    fn a_corrupt_session_log_names_the_file_and_line() {
        let dir = temp_dir();
        let mut log = SessionLog::open(dir.path()).unwrap();
        log.write(&TreeRecord::Session { at: 1, dir: "x".into(), resumed_from: None });
        drop(log);
        let mut file = fs::OpenOptions::new().append(true).open(dir.path().join("session.jsonl")).unwrap();
        file.write_all(b"{broken\n").unwrap();
        let err = read_tree(dir.path()).unwrap_err();
        assert!(err.contains("session.jsonl") && err.contains("line 2"), "{err}");
    }

    #[test]
    fn a_missing_session_log_is_an_error() {
        let dir = temp_dir();
        assert!(read_tree(dir.path()).is_err());
    }

    #[test]
    fn sessions_are_newest_first_and_carry_the_title() {
        let state = temp_dir();
        let root = state.path().join("sessions");
        fs::create_dir_all(root.join("1000-111")).unwrap();
        fs::create_dir_all(root.join("2000-222")).unwrap();
        let mut old = SessionLog::open(&root.join("1000-111")).unwrap();
        old.write(&TreeRecord::Session { at: 1000, dir: "/tmp/old".into(), resumed_from: None });
        old.write(&agent_record(1));
        old.write(&TreeRecord::Title { text: "older session".into() });
        drop(old);
        let mut new = SessionLog::open(&root.join("2000-222")).unwrap();
        new.write(&TreeRecord::Session { at: 2000, dir: "/tmp/new".into(), resumed_from: Some("1000-111".into()) });
        new.write(&agent_record(1));
        drop(new);
        // A dir without a session.jsonl never lists.
        fs::create_dir_all(root.join("3000-333")).unwrap();

        let infos = sessions(state.path());
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].id, "2000-222");
        assert_eq!(infos[0].title, None);
        assert_eq!(infos[1].id, "1000-111");
        assert_eq!(infos[1].title.as_deref(), Some("older session"));
    }

    #[test]
    fn state_and_rename_fold_onto_the_agent_that_was_introduced() {
        let dir = temp_dir();
        let mut log = SessionLog::open(dir.path()).unwrap();
        log.write(&TreeRecord::Session { at: 1, dir: "/tmp".into(), resumed_from: None });
        log.write(&agent_record(1));
        log.write(&agent_record(2));
        log.write(&TreeRecord::Rename { id: 2, name: "calm cloud 1".into() });
        log.write(&TreeRecord::State { id: 2, stopped: true, archived: false });
        log.write(&TreeRecord::Rename { id: 1, name: String::new() });
        drop(log);

        let rows = rows(dir.path()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[1].stopped && !rows[1].archived, "a stop lands on the row, not on the next resume");
        assert_eq!(rows[1].name.as_deref(), Some("calm cloud 1"));
        assert_eq!(rows[0].name, None, "an empty rename clears the name");
        assert!(!rows[0].stopped);
    }

    #[test]
    fn an_out_of_order_agent_is_refused() {
        let dir = temp_dir();
        let mut log = SessionLog::open(dir.path()).unwrap();
        log.write(&agent_record(1));
        log.write(&agent_record(3));
        drop(log);
        let err = rows(dir.path()).unwrap_err();
        assert!(err.contains("agent 3 is out of order"), "{err}");
    }

    #[test]
    fn a_torn_last_record_is_dropped_and_the_rest_replayed() {
        let dir = temp_dir();
        let mut log = SessionLog::open(dir.path()).unwrap();
        log.write(&agent_record(1));
        drop(log);
        // A crash mid-append: the last line has no newline and is not valid JSON.
        let mut file = fs::OpenOptions::new().append(true).open(dir.path().join("session.jsonl")).unwrap();
        file.write_all(br#"{"kind":"agent","id":2,"dep"#).unwrap();

        let records = read_tree(dir.path()).unwrap();
        assert_eq!(records.len(), 1, "the torn tail is dropped, the rest survives");
        assert_eq!(rows(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn a_session_without_agents_is_listed_by_neither_the_picker_nor_continue() {
        let state = temp_dir();
        let root = state.path().join("sessions");
        fs::create_dir_all(root.join("1000-111")).unwrap();
        let mut header_only = SessionLog::open(&root.join("1000-111")).unwrap();
        header_only.write(&TreeRecord::Session { at: 1000, dir: "/tmp".into(), resumed_from: None });
        drop(header_only);

        assert!(sessions(state.path()).is_empty(), "a header alone is not a resumable session");
        assert!(newest(state.path()).is_none());
        assert!(!resumable(&root.join("1000-111")));

        fs::create_dir_all(root.join("2000-222")).unwrap();
        let mut real = SessionLog::open(&root.join("2000-222")).unwrap();
        real.write(&agent_record(1));
        drop(real);
        assert!(resumable(&root.join("2000-222")));
        assert_eq!(newest(state.path()).unwrap().id, "2000-222");
    }

    #[test]
    fn a_picker_row_carries_no_control_characters() {
        let info = SessionInfo {
            id: "2000-222".into(),
            dir: "/tmp/new".into(),
            title: Some("clear the screen\u{1b}[2J and spoof".into()),
        };
        let row = picker_row(&info, 2000);
        assert!(!row.contains('\u{1b}'), "{row:?}");
        assert!(row.contains("clear the screen[2J and spoof"), "{row:?}");
    }

    #[test]
    fn picker_rows_show_id_title_and_age() {
        let info = SessionInfo { id: "2000-222".into(), dir: "/tmp/new".into(), title: Some("fix the bug".into()) };
        assert_eq!(picker_row(&info, 2000 + 125), "2000-222  fix the bug  2m ago");
        let plain = SessionInfo { id: "2000-222".into(), dir: "/tmp/new".into(), title: None };
        assert_eq!(picker_row(&plain, 2000 + 2 * 86400 + 10), "2000-222  /tmp/new  2d ago");
    }
}
