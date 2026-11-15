//! Line-anchored edits of one file, and the per-agent record of what it has read.
//!
//! `read_file` numbers lines and records a file's hash, line count and the range it showed; `edit`
//! takes those numbers as anchors and refuses a file this agent has not read or one that changed
//! since. The diff it returns is the same compact text the transcript renders.

use crate::config;
use crate::diff;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Rows one edit result may carry before the rest is only counted.
pub(crate) const MAX_DIFF_ROWS: usize = 400;

/// Rows `preview` keeps of an edit result, the rest counted in `more`.
pub(crate) const DIFF_PREVIEW_LINES: usize = 200;

/// Longest file `read_file` returns and an `edit` result may spend, leaving room for its notes.
pub(crate) const MAX_READ_BYTES: usize = 28_000;

/// Longest file `edit` opens whole: past this the model is told to work in ranges.
const MAX_EDIT_BYTES: u64 = 4 << 20;

const HASH_CHUNK: usize = 64 * 1024;

const CONTEXT_ROWS: usize = 0;

/// What one agent knows about one file: its hash when read, its line count, and the lines shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) hash: [u8; 32],
    pub(crate) lines: usize,
    pub(crate) from: usize,
    pub(crate) to: usize,
}

/// Every file one agent has read or written, keyed by canonical path.
#[derive(Debug, Default)]
pub(crate) struct Store {
    seen: HashMap<PathBuf, Seen>,
}

/// The digest of `bytes`, identical to what `hash_file` streams for the same content.
pub(crate) fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// The digest of a whole file, streamed in chunks so a read past the display budget still hashes it.
pub(crate) fn hash_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = fs::File::open(path).map_err(|e| config::io_error(path.display(), &e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_CHUNK];
    loop {
        let read = file.read(&mut buffer).map_err(|e| config::io_error(path.display(), &e))?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or_default());
    }
    Ok(hasher.finalize().into())
}

impl Store {
    /// Records `file` as seen from `from` to `to`, with the hash its current bytes have.
    pub(crate) fn record(&mut self, file: PathBuf, hash: [u8; 32], lines: usize, from: usize, to: usize) {
        self.seen.insert(file, Seen { hash, lines, from, to });
    }

    /// Drops what this agent knows about `file`: after a removal, or once a move retires its name.
    pub(crate) fn forget(&mut self, file: &Path) {
        self.seen.remove(file);
    }

    /// The entry `edit` needs, or the refusal that sends the model back to `read_file`.
    pub(crate) fn check(&self, file: &Path, display: &str, hash: &[u8; 32]) -> Result<Seen, String> {
        let Some(seen) = self.seen.get(file) else {
            return Err(format!("read {display} before editing it"));
        };
        if seen.hash != *hash {
            return Err(format!("{display} changed since you read it; read it again"));
        }
        Ok(*seen)
    }
}

/// One operation of a patch, with the original anchors it was written against.
enum Operation {
    Replace {
        from: usize,
        to: usize,
        body: Vec<String>,
    },
    InsertBefore {
        at: usize,
        body: Vec<String>,
    },
    /// `at == usize::MAX` is `>$`: append at the tail.
    InsertAfter {
        at: usize,
        body: Vec<String>,
    },
    Cut {
        from: usize,
        to: usize,
    },
    Remove,
    Move(String),
}

/// Applies `patch` to `working_dir/path`, refusing anything the agent has not read.
pub(crate) fn apply(working_dir: &Path, path: &str, changes: &str, store: &mut Store) -> Result<String, String> {
    if crate::url::is_url(path) {
        let scheme = path.split_once("://").map_or("", |(scheme, _)| scheme);
        return Err(format!("edit works on files; use write_file for {scheme}://"));
    }
    let full = working_dir.join(path);
    let metadata = match fs::metadata(&full) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{path} does not exist; use write_file to create it"));
        }
        Err(e) => return Err(config::io_error(path, &e)),
    };
    if metadata.is_dir() {
        return Err(format!("{path} is a directory"));
    }
    if metadata.len() > MAX_EDIT_BYTES {
        return Err(format!("{path} is {} bytes, past the {MAX_EDIT_BYTES}-byte edit budget", metadata.len()));
    }
    let bytes = fs::read(&full).map_err(|e| config::io_error(path, &e))?;
    let old = String::from_utf8(bytes.clone()).map_err(|_| format!("{path}: not valid UTF-8"))?;
    let canonical = fs::canonicalize(&full).map_err(|e| config::io_error(path, &e))?;
    let seen = store.check(&canonical, path, &hash(&bytes))?;

    let operations = parse(path, changes)?;
    validate(&operations, &seen)?;
    let destination = operations.iter().find_map(|operation| match operation {
        Operation::Move(destination) => Some(destination.clone()),
        _ => None,
    });
    if let Some(destination) = &destination {
        let target = working_dir.join(destination);
        match fs::canonicalize(&target) {
            Ok(existing) if existing == canonical => return Err("a file cannot be moved onto itself".into()),
            Ok(_) => return Err(format!("destination {destination} already exists")),
            Err(_) => {}
        }
    }
    let removed = operations.iter().any(|operation| matches!(operation, Operation::Remove));

    let mut rows: Vec<String> = diff::split_lines(&old).into_iter().map(str::to_string).collect();
    let mut delta: i64 = 0;
    for operation in &operations {
        match operation {
            Operation::Replace { from, to, body } => {
                let start = position(*from, -1, delta)?;
                let end = position(*to, 0, delta)?;
                splice(&mut rows, start, end, body)?;
                let dropped = span(*to, *from)?;
                let added = rows_len(body)?;
                delta = delta.checked_sub(dropped).and_then(|delta| delta.checked_add(added)).ok_or_else(arithmetic)?;
            }
            Operation::InsertBefore { at, body } => {
                let at = position(*at, -1, delta)?;
                splice(&mut rows, at, at, body)?;
                delta = delta.checked_add(rows_len(body)?).ok_or_else(arithmetic)?;
            }
            Operation::InsertAfter { at, body } => {
                let at = if *at == usize::MAX { rows.len() } else { position(*at, 0, delta)? };
                splice(&mut rows, at, at, body)?;
                delta = delta.checked_add(rows_len(body)?).ok_or_else(arithmetic)?;
            }
            Operation::Cut { from, to } => {
                let start = position(*from, -1, delta)?;
                let end = position(*to, 0, delta)?;
                splice(&mut rows, start, end, &[])?;
                delta = delta.checked_sub(span(*to, *from)?).ok_or_else(arithmetic)?;
            }
            Operation::Remove | Operation::Move(_) => {}
        }
    }
    let new = if old.ends_with('\n') && !rows.is_empty() { format!("{}\n", rows.join("\n")) } else { rows.join("\n") };
    let difference = if removed { diff::lines(&old, "") } else { diff::lines(&old, &new) };
    let structural = removed || destination.is_some();
    if difference.added == 0 && difference.removed == 0 && !structural {
        return Err(format!("no change: the patch leaves {path} as it is"));
    }
    let trailer = if removed {
        Some(format!("removed {path}"))
    } else {
        destination.as_ref().map(|destination| format!("moved to {destination}"))
    };
    let result = render(&difference, trailer);

    if removed {
        fs::remove_file(&full).map_err(|e| config::io_error(path, &e))?;
        store.forget(&canonical);
        return Ok(result);
    }
    write_atomic(&full, new.as_bytes(), path)?;
    let lines = diff::line_count(&new);
    let (from, to) = if lines == 0 { (0, 0) } else { (1, lines) };
    if let Some(destination) = destination {
        let target = working_dir.join(&destination);
        fs::rename(&full, &target).map_err(|e| config::io_error(&destination, &e))?;
        let moved = fs::canonicalize(&target).map_err(|e| config::io_error(&destination, &e))?;
        store.forget(&canonical);
        store.record(moved, hash(new.as_bytes()), lines, from, to);
    } else {
        store.record(canonical, hash(new.as_bytes()), lines, from, to);
    }
    Ok(result)
}

/// The verdict row, the compact diff, then one plain row for a removal or a move.
fn render(difference: &diff::Diff, trailer: Option<String>) -> String {
    let rendered = diff::render(difference, CONTEXT_ROWS);
    let mut out = format!("edit: +{}/-{}", difference.added, difference.removed);
    let mut bytes = out.len();
    let mut kept = 0;
    for row in &rendered {
        let cost = row.len().saturating_add(1);
        if kept >= MAX_DIFF_ROWS || bytes.saturating_add(cost) > MAX_READ_BYTES.saturating_sub(64) {
            break;
        }
        out.push('\n');
        out.push_str(row);
        bytes = bytes.saturating_add(cost);
        kept += 1;
    }
    let hidden = rendered.len().saturating_sub(kept);
    if hidden > 0 {
        let _ = write!(out, "\n… {hidden} more rows");
    }
    if let Some(trailer) = trailer {
        out.push('\n');
        out.push_str(&trailer);
    }
    out
}

/// Writes through a sibling temporary file, so a failed write never leaves a half-written target.
fn write_atomic(target: &Path, bytes: &[u8], display: &str) -> Result<(), String> {
    let name = target.file_name().map_or_default(|name| name.to_string_lossy().into_owned());
    let temporary = target.with_file_name(format!("{name}.edit-{}", std::process::id()));
    if let Err(e) = fs::write(&temporary, bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(config::io_error(display, &e));
    }
    if let Err(e) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(config::io_error(display, &e));
    }
    Ok(())
}

/// Parses a patch, its optional `[path]` header first.
fn parse(path: &str, changes: &str) -> Result<Vec<Operation>, String> {
    let mut operations: Vec<Operation> = Vec::new();
    let mut body: Option<usize> = None;
    let mut first = true;
    for (index, raw) in changes.lines().enumerate() {
        let number = index.saturating_add(1);
        if let Some(slot) = body
            && let Some(text) = raw.strip_prefix('+')
        {
            if let Some(operation) = operations.get_mut(slot) {
                push_body(operation, text);
            }
            continue;
        }
        body = None;
        if raw.trim().is_empty() {
            continue;
        }
        if first && raw.starts_with('[') && raw.ends_with(']') {
            first = false;
            let header = raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default();
            if header != path {
                return Err(format!("patch header [{header}] does not match path {path}"));
            }
            continue;
        }
        first = false;
        if raw.strip_prefix('+').is_some() {
            return Err(row_error(number, raw));
        }
        if raw == "REM" {
            operations.push(Operation::Remove);
            continue;
        }
        if let Some(destination) = raw.strip_prefix("MV ") {
            let destination = unquote(destination.trim());
            if destination.is_empty() {
                return Err(row_error(number, raw));
            }
            operations.push(Operation::Move(destination.to_string()));
            continue;
        }
        let Some(rest) = raw.strip_prefix("CUT ").or_else(|| raw.strip_prefix("PUT ")) else {
            return Err(row_error(number, raw));
        };
        if raw.contains('*') {
            return Err("block ops (`*`) need a syntax tree; use `PUT N.=M:`".into());
        }
        if raw.contains('@') {
            return Err("registers (@name) are not supported".into());
        }
        if raw.starts_with("CUT ") {
            let range = rest.trim().strip_suffix(':').unwrap_or(rest.trim());
            let (from, to) = range_of(range, number, raw)?;
            operations.push(Operation::Cut { from, to });
            continue;
        }
        if let Some(rest) = rest.strip_prefix('<') {
            let at = rest.strip_suffix(':').and_then(parse_number).ok_or_else(|| row_error(number, raw))?;
            operations.push(Operation::InsertBefore { at, body: Vec::new() });
            body = Some(operations.len().saturating_sub(1));
            continue;
        }
        if let Some(rest) = rest.strip_prefix('>') {
            let at = rest.strip_suffix(':').ok_or_else(|| row_error(number, raw))?;
            let at =
                if at.trim() == "$" { usize::MAX } else { parse_number(at).ok_or_else(|| row_error(number, raw))? };
            operations.push(Operation::InsertAfter { at, body: Vec::new() });
            body = Some(operations.len().saturating_sub(1));
            continue;
        }
        let range = rest.strip_suffix(':').ok_or_else(|| row_error(number, raw))?;
        let (from, to) = range_of(range, number, raw)?;
        operations.push(Operation::Replace { from, to, body: Vec::new() });
        body = Some(operations.len().saturating_sub(1));
    }
    if operations.is_empty() {
        return Err("the patch has no operations".into());
    }
    let removals = operations.iter().any(|operation| matches!(operation, Operation::Remove));
    let moves = operations.iter().any(|operation| matches!(operation, Operation::Move(_)));
    if removals && moves {
        return Err("REM and MV cannot be combined".into());
    }
    Ok(operations)
}

/// The body rows of a PUT reach the operation they belong to.
fn push_body(operation: &mut Operation, text: &str) {
    match operation {
        Operation::Replace { body, .. }
        | Operation::InsertBefore { body, .. }
        | Operation::InsertAfter { body, .. } => body.push(text.to_string()),
        Operation::Cut { .. } | Operation::Remove | Operation::Move(_) => {}
    }
}

/// `N.=M`, both anchors 1-based and ordered.
fn range_of(range: &str, number: usize, raw: &str) -> Result<(usize, usize), String> {
    let Some((from, to)) = range.split_once(".=") else {
        return Err(row_error(number, raw));
    };
    let from = parse_number(from).ok_or_else(|| row_error(number, raw))?;
    let to = parse_number(to).ok_or_else(|| row_error(number, raw))?;
    if from > to {
        return Err(format!("ops must name an ordered range ({from}..={to}, not {to}..={from})"));
    }
    Ok((from, to))
}

fn parse_number(text: &str) -> Option<usize> {
    text.trim().parse().ok()
}

/// Strips one pair of matching quotes from a destination.
fn unquote(text: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = text.strip_prefix(quote).and_then(|rest| rest.strip_suffix(quote)) {
            return inner;
        }
    }
    text
}

fn row_error(number: usize, raw: &str) -> String {
    format!("row {number} is not a patch row: `{raw}`")
}

/// Checks anchors against each other and against what the agent saw.
fn validate(operations: &[Operation], seen: &Seen) -> Result<(), String> {
    let mut previous: Option<usize> = None;
    let mut range: Option<(usize, usize)> = None;
    for operation in operations {
        let anchor = match operation {
            Operation::Replace { from, .. } | Operation::Cut { from, .. } => *from,
            Operation::InsertBefore { at, .. } | Operation::InsertAfter { at, .. } => *at,
            Operation::Remove | Operation::Move(_) => previous.unwrap_or(0),
        };
        if let Some(previous) = previous
            && anchor < previous
        {
            return Err(format!("ops must be in ascending line order (line {anchor} after line {previous})"));
        }
        previous = Some(anchor);
        match operation {
            Operation::Replace { from, to, .. } | Operation::Cut { from, to, .. } => {
                if let Some((first, last)) = range
                    && *from <= last
                {
                    return Err(format!("ops overlap: line {from} is inside {first}..={last}"));
                }
                range = Some((*from, *to));
                check_range(*from, *to, seen)?;
            }
            Operation::InsertBefore { at, .. } => {
                if seen.lines == 0 && *at == 1 {
                    continue;
                }
                check_line(*at, seen)?;
            }
            Operation::InsertAfter { at, .. } => {
                if *at != usize::MAX {
                    check_line(*at, seen)?;
                }
            }
            Operation::Remove | Operation::Move(_) => {}
        }
    }
    Ok(())
}

fn outside(line: usize, seen: &Seen) -> String {
    if seen.lines == 0 {
        format!("line {line} is outside what you read (you saw nothing, the file is empty)")
    } else {
        format!("line {line} is outside what you read (you saw {}-{}); read the range first", seen.from, seen.to)
    }
}

fn check_line(line: usize, seen: &Seen) -> Result<(), String> {
    if line == 0 || line < seen.from || line > seen.to {
        return Err(outside(line, seen));
    }
    Ok(())
}

fn check_range(from: usize, to: usize, seen: &Seen) -> Result<(), String> {
    if from == 0 || from < seen.from {
        return Err(outside(from, seen));
    }
    if to > seen.to {
        return Err(outside(to, seen));
    }
    Ok(())
}

/// An original anchor translated through the rows the earlier operations added or removed.
fn position(anchor: usize, shift: i64, delta: i64) -> Result<usize, String> {
    let anchor = i64::try_from(anchor).map_err(|_| arithmetic())?;
    let raw = anchor.checked_add(shift).and_then(|raw| raw.checked_add(delta)).ok_or_else(arithmetic)?;
    usize::try_from(raw).map_err(|_| arithmetic())
}

/// Rows a closed range holds.
fn span(to: usize, from: usize) -> Result<i64, String> {
    let to = i64::try_from(to).map_err(|_| arithmetic())?;
    let from = i64::try_from(from).map_err(|_| arithmetic())?;
    to.checked_sub(from).and_then(|span| span.checked_add(1)).ok_or_else(arithmetic)
}

fn rows_len(body: &[String]) -> Result<i64, String> {
    i64::try_from(body.len()).map_err(|_| arithmetic())
}

fn arithmetic() -> String {
    "edit arithmetic overflow".into()
}

/// Replaces `start..end` with `body`, refusing indices that left the vector.
fn splice(rows: &mut Vec<String>, start: usize, end: usize, body: &[String]) -> Result<(), String> {
    if start > end || end > rows.len() {
        return Err(arithmetic());
    }
    rows.splice(start..end, body.iter().cloned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_for(dir: &Path, path: &str, text: &str) -> Store {
        let mut store = Store::default();
        let full = dir.join(path);
        store.record(
            fs::canonicalize(&full).unwrap(),
            hash(text.as_bytes()),
            diff::line_count(text),
            usize::from(!text.is_empty()),
            diff::line_count(text),
        );
        store
    }

    fn write(dir: &Path, path: &str, text: &str) {
        fs::write(dir.join(path), text).unwrap();
    }

    #[test]
    fn replaces_a_line_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\nb\nc\n");
        let mut store = store_for(dir.path(), "t.rs", "a\nb\nc\n");
        let out = apply(dir.path(), "t.rs", "PUT 2.=2:\n+B\n", &mut store).unwrap();
        assert_eq!(out, "edit: +1/-1\n-2|b\n+2|B");
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn inserts_at_head_middle_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\nb\n");
        let mut store = store_for(dir.path(), "t.rs", "a\nb\n");
        apply(dir.path(), "t.rs", "PUT <1:\n+head\nPUT >1:\n+mid\nPUT >$:\n+tail\n", &mut store).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "head\na\nmid\nb\ntail\n");
    }

    #[test]
    fn cuts_a_range() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\nb\nc\nd\n");
        let mut store = store_for(dir.path(), "t.rs", "a\nb\nc\nd\n");
        let out = apply(dir.path(), "t.rs", "CUT 2.=3", &mut store).unwrap();
        assert_eq!(out, "edit: +0/-2\n-2|b\n-3|c");
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\nd\n");
    }

    #[test]
    fn escapes_a_literal_marker() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        apply(dir.path(), "t.rs", "PUT >$:\n+-minus\n++plus\n+", &mut store).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\n-minus\n+plus\n\n");
    }

    #[test]
    fn removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\nb\n");
        let mut store = store_for(dir.path(), "t.rs", "a\nb\n");
        let out = apply(dir.path(), "t.rs", "REM", &mut store).unwrap();
        assert_eq!(out, "edit: +0/-2\n-1|a\n-2|b\nremoved t.rs");
        assert!(!dir.path().join("t.rs").exists());
    }

    #[test]
    fn moves_the_file_without_keeping_the_old_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        let out = apply(dir.path(), "t.rs", "MV renamed.rs", &mut store).unwrap();
        assert!(out.ends_with("moved to renamed.rs"), "{out}");
        assert!(!dir.path().join("t.rs").exists());
        assert_eq!(fs::read_to_string(dir.path().join("renamed.rs")).unwrap(), "a\n");
        let moved = fs::canonicalize(dir.path().join("renamed.rs")).unwrap();
        assert!(store.seen.contains_key(&moved));
    }

    #[test]
    fn a_move_onto_an_existing_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        write(dir.path(), "other.rs", "b\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        let out = apply(dir.path(), "t.rs", "MV other.rs", &mut store).unwrap_err();
        assert_eq!(out, "destination other.rs already exists");
    }

    #[test]
    fn a_move_onto_itself_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        let out = apply(dir.path(), "t.rs", "MV t.rs", &mut store).unwrap_err();
        assert_eq!(out, "a file cannot be moved onto itself");
    }

    #[test]
    fn an_unread_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = Store::default();
        let out = apply(dir.path(), "t.rs", "PUT 1.=1:\n+b\n", &mut store).unwrap_err();
        assert_eq!(out, "read t.rs before editing it");
    }

    #[test]
    fn a_stale_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        write(dir.path(), "t.rs", "a\nb\n");
        let out = apply(dir.path(), "t.rs", "PUT 1.=1:\n+z\n", &mut store).unwrap_err();
        assert_eq!(out, "t.rs changed since you read it; read it again");
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\nb\n");
    }

    #[test]
    fn an_unseen_anchor_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\nb\nc\n");
        let mut store = Store::default();
        store.record(fs::canonicalize(dir.path().join("t.rs")).unwrap(), hash(b"a\nb\nc\n"), 3, 2, 3);
        let out = apply(dir.path(), "t.rs", "PUT 1.=1:\n+z\n", &mut store).unwrap_err();
        assert_eq!(out, "line 1 is outside what you read (you saw 2-3); read the range first");
        let out = apply(dir.path(), "t.rs", "PUT 3.=3:\n+C\n", &mut store).unwrap();
        assert_eq!(out, "edit: +1/-1\n-3|c\n+3|C");
    }

    #[test]
    fn a_no_op_patch_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        let out = apply(dir.path(), "t.rs", "PUT 1.=1:\n+a\n", &mut store).unwrap_err();
        assert_eq!(out, "no change: the patch leaves t.rs as it is");
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\n");
    }

    #[test]
    fn a_descending_anchor_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        let mut store = store_for(dir.path(), "t.rs", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        let out = apply(dir.path(), "t.rs", "PUT 9.=9:\n+x\nPUT 5.=5:\n+y\n", &mut store).unwrap_err();
        assert_eq!(out, "ops must be in ascending line order (line 5 after line 9)");
    }

    #[test]
    fn overlapping_ranges_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        let mut store = store_for(dir.path(), "t.rs", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        let out = apply(dir.path(), "t.rs", "PUT 1.=6:\n+x\nCUT 4.=5", &mut store).unwrap_err();
        assert_eq!(out, "ops overlap: line 4 is inside 1..=6");
    }

    #[test]
    fn a_reversed_range_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        let mut store = store_for(dir.path(), "t.rs", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
        let out = apply(dir.path(), "t.rs", "PUT 9.=5:\n+x\n", &mut store).unwrap_err();
        assert_eq!(out, "ops must name an ordered range (9..=5, not 5..=9)");
    }

    #[test]
    fn an_unrecognised_row_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        let out = apply(dir.path(), "t.rs", "PUT 1.=1:\n+z\n-text\n", &mut store).unwrap_err();
        assert_eq!(out, "row 3 is not a patch row: `-text`");
    }

    #[test]
    fn block_ops_and_registers_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        let out = apply(dir.path(), "t.rs", "PUT 1*3:\n+x\n", &mut store).unwrap_err();
        assert_eq!(out, "block ops (`*`) need a syntax tree; use `PUT N.=M:`");
        let out = apply(dir.path(), "t.rs", "PUT 1.=1 @one:\n+x\n", &mut store).unwrap_err();
        assert_eq!(out, "registers (@name) are not supported");
    }

    #[test]
    fn a_header_naming_another_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "t.rs", "a\n");
        let mut store = store_for(dir.path(), "t.rs", "a\n");
        let out = apply(dir.path(), "t.rs", "[other.rs]\nPUT 1.=1:\n+x\n", &mut store).unwrap_err();
        assert_eq!(out, "patch header [other.rs] does not match path t.rs");
        let out = apply(dir.path(), "t.rs", "[t.rs]\nPUT 1.=1:\n+x\n", &mut store).unwrap();
        assert!(out.starts_with("edit: +1/-1"), "{out}");
    }

    #[test]
    fn a_directory_and_a_missing_file_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::default();
        let out = apply(dir.path(), "missing.rs", "PUT 1.=1:\n+x\n", &mut store).unwrap_err();
        assert_eq!(out, "missing.rs does not exist; use write_file to create it");
        let out = apply(dir.path(), ".", "PUT 1.=1:\n+x\n", &mut store).unwrap_err();
        assert_eq!(out, ". is a directory");
    }

    #[test]
    fn a_url_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::default();
        let out = apply(dir.path(), "conflict://1", "PUT 1.=1:\n+x\n", &mut store).unwrap_err();
        assert_eq!(out, "edit works on files; use write_file for conflict://");
    }
}
