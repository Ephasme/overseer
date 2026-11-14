//! Merge-conflict blocks a worker resolves by id.
//!
//! A read of a file carrying `<<<<<<<` markers gives every block an id, and a write to
//! `conflict://<N>` swaps just that block's marker lines for the replacement. The lines around it
//! stay put, so resolving never means retyping the file and getting the context wrong.

use crate::config;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Prefix that makes a tool path a conflict reference rather than a file.
pub(crate) const SCHEME: &str = "conflict://";

/// Suffix that lists a file's blocks without printing its text.
pub(crate) const LIST_SUFFIX: &str = ":conflicts";

/// Widest run of lines a replacement may repeat from around the block before they are dropped.
const CONTEXT: usize = 12;

const OPEN: &str = "<<<<<<<";
const BASE: &str = "|||||||";
const SEPARATOR: &str = "=======";
const CLOSE: &str = ">>>>>>>";

/// One unresolved block, both sides in hand, so resolving needs no second read.
#[derive(Clone)]
struct Block {
    id: usize,
    path: PathBuf,
    display: String,
    /// 1-based line of the `<<<<<<<` marker.
    start: usize,
    /// 1-based line of the `>>>>>>>` marker.
    end: usize,
    /// The block verbatim, markers included: what a later write locates it by.
    raw: Vec<String>,
    ours: Vec<String>,
    theirs: Vec<String>,
    base: Option<Vec<String>>,
    ours_label: String,
    theirs_label: String,
}

/// Every block registered in one worker's session. An id is never reused, so a stale write fails
/// loudly instead of hitting a block the model never saw.
#[derive(Default)]
pub(crate) struct Registry {
    next_id: usize,
    blocks: Vec<Block>,
}

/// What one resolution did, for the text the model reads back.
struct Outcome {
    path: PathBuf,
    display: String,
    range: String,
    dropped: usize,
}

impl Registry {
    /// Re-registers `path` from `text`: blocks still present keep their id, vanished ones are
    /// dropped, new ones get the next id.
    pub(crate) fn reconcile(&mut self, path: &Path, display: &str, text: &str) {
        let found = scan(text);
        let (elsewhere, here): (Vec<Block>, Vec<Block>) =
            std::mem::take(&mut self.blocks).into_iter().partition(|block| block.path != path);
        let mut reusable: HashMap<Vec<String>, Vec<Block>> = HashMap::new();
        for block in here {
            reusable.entry(block.raw.clone()).or_default().push(block);
        }
        let mut kept = elsewhere;
        for scanned in found {
            // Blocks with identical text are interchangeable, so any of them can take the id.
            let known = reusable
                .get_mut(&scanned.raw)
                .and_then(|queue| if queue.is_empty() { None } else { Some(queue.remove(0)) });
            let Some(block) = known else {
                self.next_id += 1;
                kept.push(Block {
                    id: self.next_id,
                    path: path.to_path_buf(),
                    display: display.to_string(),
                    start: scanned.start,
                    end: scanned.end,
                    raw: scanned.raw,
                    ours: scanned.ours,
                    theirs: scanned.theirs,
                    base: scanned.base,
                    ours_label: scanned.ours_label,
                    theirs_label: scanned.theirs_label,
                });
                continue;
            };
            let block = Block { start: scanned.start, end: scanned.end, display: display.to_string(), ..block };
            kept.push(block);
        }
        self.blocks = kept;
    }

    /// The notice a read appends when `path` still holds unresolved blocks; empty when it holds none.
    pub(crate) fn summary(&self, path: &Path) -> String {
        let mine: Vec<&Block> = self.blocks.iter().filter(|block| block.path == path).collect();
        let Some(first) = mine.first() else {
            return String::new();
        };
        let mut out = format!(
            "⚠ {} unresolved conflict{} in {} — `write_file` on a block's `conflict://<N>` path replaces only its marker lines, so never repeat the code around it:\n",
            mine.len(),
            if mine.len() == 1 { "" } else { "s" },
            first.display
        );
        for block in mine {
            let _ = writeln!(
                out,
                "- #{}: {} (ours {}, theirs {})",
                block.id,
                range(block),
                quote(&block.ours_label),
                quote(&block.theirs_label)
            );
        }
        out.push_str(
            "`content` may be a literal replacement or @ours / @theirs / @both. Many at once: `write_file({ \"path\": \"conflict://*\", \"content\": \"1: @ours\\n2: @theirs\" })`. Inspect one: `read_file({ \"path\": \"conflict://<N>\" })`, or `/ours`, `/theirs`, `/base` for a single side.\n",
        );
        out
    }

    /// Every registered block, across files: what a bulk write reports as still pending.
    fn pending(&self) -> String {
        if self.blocks.is_empty() {
            return String::new();
        }
        let listed: Vec<String> =
            self.blocks.iter().map(|block| format!("#{} in {} {}", block.id, block.display, range(block))).collect();
        format!("still registered: {}\n", listed.join(", "))
    }

    fn get(&self, id: usize) -> Option<&Block> {
        self.blocks.iter().find(|block| block.id == id)
    }

    fn unknown(&self, id: usize) -> String {
        if self.blocks.is_empty() {
            return format!("conflict #{id} is not registered: read a file carrying conflict markers first");
        }
        let listed: Vec<String> =
            self.blocks.iter().map(|block| format!("#{} in {}", block.id, block.display)).collect();
        format!(
            "conflict #{id} is not registered — it was resolved, or no read has shown its block yet; registered: {}",
            listed.join(", ")
        )
    }

    /// Renders `conflict://<N>` or `conflict://<N>/<scope>` for the model.
    pub(crate) fn read(&self, rest: &str) -> Result<String, String> {
        if rest == "*" {
            return Err(format!(
                "`{SCHEME}*` is write-only: read `{SCHEME}<N>` for one block, or `{LIST_SUFFIX}` on a file for its list"
            ));
        }
        let (id, scope) = parse(rest)?;
        let block = self.get(id).ok_or_else(|| self.unknown(id))?;
        let text = match scope {
            Some(scope) => side(block, scope)?.join("\n"),
            None => block.raw.join("\n"),
        };
        Ok(format!("conflict #{} in {}, {}\n{text}", block.id, block.display, range(block)))
    }

    /// Resolves `conflict://<N>` or `conflict://*` with `content`.
    pub(crate) fn write(&mut self, rest: &str, content: &str) -> Result<String, String> {
        if rest == "*" {
            return self.write_all(content);
        }
        let (id, scope) = parse(rest)?;
        if let Some(scope) = scope {
            return Err(format!(
                "`{SCHEME}{id}/{scope}` is read-only: read it to inspect that side, then write `{SCHEME}{id}` with the chosen content (or `@{scope}`)"
            ));
        }
        let outcome = self.apply(id, content)?;
        let mut out = format!("resolved conflict #{} at {} in {}", id, outcome.range, outcome.display);
        if outcome.dropped > 0 {
            let _ = write!(
                out,
                "\nnote: dropped {} replacement line(s) that repeated the code around the block — a write replaces only the marker block",
                outcome.dropped
            );
        }
        let summary = self.summary(&outcome.path);
        if !summary.is_empty() {
            out.push('\n');
            out.push_str(&summary);
        }
        Ok(out)
    }

    /// Resolves several blocks in one call: one `<id>: @side` line each, or one side for every block.
    fn write_all(&mut self, content: &str) -> Result<String, String> {
        if self.blocks.is_empty() {
            return Err("no conflicts are registered: read the file holding them first".to_string());
        }
        let token = content.trim();
        let plan: Vec<(usize, String)> = if matches!(token, "@ours" | "@theirs" | "@base" | "@both") {
            self.blocks.iter().map(|block| (block.id, token.to_string())).collect()
        } else {
            directives(content)?
        };
        if let Some(missing) = plan.iter().map(|(id, _)| *id).find(|id| self.get(*id).is_none()) {
            return Err(self.unknown(missing));
        }
        let mut done = Vec::new();
        for (id, side) in &plan {
            self.apply(*id, side)?;
            done.push(format!("#{id}"));
        }
        let mut out = format!("resolved {} conflict(s): {}", done.len(), done.join(", "));
        let pending = self.pending();
        if !pending.is_empty() {
            out.push('\n');
            out.push_str(&pending);
        }
        Ok(out)
    }

    /// Replaces one block's marker lines in its file, then re-registers what the file still holds.
    fn apply(&mut self, id: usize, content: &str) -> Result<Outcome, String> {
        let block = self.get(id).ok_or_else(|| self.unknown(id))?.clone();
        let text = fs::read_to_string(&block.path).map_err(|e| config::io_error(&block.display, &e))?;
        let replacement = pick(&block, content)?;
        let (updated, dropped) = splice(&text, &block, &replacement)?;
        fs::write(&block.path, &updated).map_err(|e| config::io_error(&block.display, &e))?;
        self.reconcile(&block.path, &block.display, &updated);
        let range = range(&block);
        Ok(Outcome { path: block.path, display: block.display, range, dropped })
    }
}

/// A block as it sits in the file, before it is given an id.
struct Scanned {
    start: usize,
    end: usize,
    raw: Vec<String>,
    ours: Vec<String>,
    theirs: Vec<String>,
    base: Option<Vec<String>>,
    ours_label: String,
    theirs_label: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Ours,
    Base,
    Theirs,
}

/// Every conflict block in `text`, in file order. An unterminated block is not a conflict.
fn scan(text: &str) -> Vec<Scanned> {
    let lines: Vec<&str> = text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    let mut found = Vec::new();
    let mut index = 0;
    while let Some(first) = lines.get(index) {
        if !labeled(first, OPEN) {
            index += 1;
            continue;
        }
        let start = index;
        let mut ours = Vec::new();
        let mut theirs = Vec::new();
        let mut base: Option<Vec<String>> = None;
        let mut section = Section::Ours;
        let mut closed = None;
        index += 1;
        while let Some(line) = lines.get(index) {
            if labeled(line, CLOSE) {
                closed = Some(index);
                index += 1;
                break;
            }
            if section == Section::Ours && labeled(line, BASE) {
                section = Section::Base;
                base = Some(Vec::new());
            } else if section != Section::Theirs && line.trim_end() == SEPARATOR {
                section = Section::Theirs;
            } else {
                match section {
                    Section::Ours => ours.push((*line).to_string()),
                    Section::Base => base.get_or_insert_with(Vec::new).push((*line).to_string()),
                    Section::Theirs => theirs.push((*line).to_string()),
                }
            }
            index += 1;
        }
        let Some(end) = closed else {
            break;
        };
        let raw: Vec<String> =
            lines.get(start..=end).unwrap_or_default().iter().map(|line| (*line).to_string()).collect();
        let theirs_label = lines.get(end).map_or_default(|line| label_of(line));
        found.push(Scanned {
            start: start + 1,
            end: end + 1,
            raw,
            ours,
            theirs,
            base,
            ours_label: label_of(first),
            theirs_label,
        });
    }
    found
}

/// A marker line: the marker alone, or the marker and the branch it names (`<<<<<<< HEAD`).
fn labeled(line: &str, marker: &str) -> bool {
    line.strip_prefix(marker).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// The branch a marker line names, or nothing when it carries no label.
fn label_of(line: &str) -> String {
    line.split_once(' ').map_or_default(|(_, label)| label.trim().to_string())
}

/// `conflict://<rest>` split into its id and its optional read-only scope.
fn parse(rest: &str) -> Result<(usize, Option<&str>), String> {
    let (id, scope) = rest.split_once('/').map_or((rest, None), |(id, scope)| (id, Some(scope)));
    let Ok(id) = id.parse::<usize>() else {
        return Err(format!("`{SCHEME}{rest}` is not a conflict reference: use `{SCHEME}<N>`, a positive integer id"));
    };
    if id == 0 {
        return Err(format!("`{SCHEME}{rest}`: ids start at 1"));
    }
    if let Some(scope) = scope
        && !matches!(scope, "ours" | "theirs" | "base")
    {
        return Err(format!("`/{scope}` is not a conflict scope: use /ours, /theirs or /base"));
    }
    Ok((id, scope))
}

/// One side of a block, for a scoped read.
fn side<'a>(block: &'a Block, scope: &str) -> Result<&'a [String], String> {
    match scope {
        "ours" => Ok(&block.ours),
        "theirs" => Ok(&block.theirs),
        _ => block.base.as_deref().ok_or_else(|| {
            format!("conflict #{} has no base section (a 2-way merge): read /ours or /theirs", block.id)
        }),
    }
}

/// The replacement a write asks for: one of the shorthand tokens, or the literal content.
fn pick(block: &Block, content: &str) -> Result<Vec<String>, String> {
    match content.trim() {
        "@ours" => Ok(block.ours.clone()),
        "@theirs" => Ok(block.theirs.clone()),
        "@both" => Ok(block.ours.iter().chain(&block.theirs).cloned().collect()),
        "@base" => block.base.clone().ok_or_else(|| {
            format!("conflict #{} has no base section (a 2-way merge): use @ours, @theirs or @both", block.id)
        }),
        _ => Ok(body(content)),
    }
}

/// Content as lines, without the single trailing newline a file body usually ends with.
fn body(content: &str) -> Vec<String> {
    let mut lines: Vec<String> =
        content.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_string()).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Replaces the block's marker lines in `text`, and reports how many replacement lines repeated
/// the code around the block and were dropped.
fn splice(text: &str, block: &Block, replacement: &[String]) -> Result<(String, usize), String> {
    let crlf = text.split('\n').next().is_some_and(|line| line.ends_with('\r'));
    let lines: Vec<&str> = text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    let start = find(&lines, &block.raw).ok_or_else(|| {
        format!(
            "conflict #{} is no longer in {}: the file changed since the block was registered, so read it again",
            block.id, block.display
        )
    })?;
    let end = start + block.raw.len();
    let before = lines.get(..start).unwrap_or_default();
    let after = lines.get(end..).unwrap_or_default();
    let (lead, trail) = overlap(replacement, before, after);
    let kept = replacement.get(lead..replacement.len().saturating_sub(trail)).unwrap_or_default();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + kept.len());
    out.extend(before);
    out.extend(kept.iter().map(String::as_str));
    out.extend(after);
    Ok((out.join(if crlf { "\r\n" } else { "\n" }), lead + trail))
}

/// Where `raw` sits in `lines`, as a start index.
fn find(lines: &[&str], raw: &[String]) -> Option<usize> {
    if raw.is_empty() || raw.len() > lines.len() {
        return None;
    }
    (0..=lines.len() - raw.len())
        .find(|start| lines.iter().skip(*start).zip(raw).all(|(line, want)| *line == want.as_str()))
}

/// The runs of `replacement` that repeat the lines just before and just after the block.
fn overlap(replacement: &[String], before: &[&str], after: &[&str]) -> (usize, usize) {
    let replace: Vec<&str> = replacement.iter().map(String::as_str).collect();
    let widest = CONTEXT.min(before.len()).min(replace.len());
    let lead = (1..=widest).rev().find(|count| tail(before, *count) == head(&replace, *count)).unwrap_or(0);
    let rest = replace.get(lead..).unwrap_or_default();
    let widest = CONTEXT.min(after.len()).min(rest.len());
    let trail = (1..=widest).rev().find(|count| head(after, *count) == tail(rest, *count)).unwrap_or(0);
    (lead, trail)
}

fn head<'a>(lines: &[&'a str], count: usize) -> Vec<&'a str> {
    lines.iter().take(count).copied().collect()
}

fn tail<'a>(lines: &[&'a str], count: usize) -> Vec<&'a str> {
    lines.iter().skip(lines.len().saturating_sub(count)).copied().collect()
}

/// `<id>: @side` lines a bulk write may carry.
fn directives(content: &str) -> Result<Vec<(usize, String)>, String> {
    let mut plan = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed = line.split_once(':').and_then(|(id, side)| {
            let id = id.trim().trim_start_matches('#').parse::<usize>().ok()?;
            let side = side.trim();
            matches!(side, "@ours" | "@theirs" | "@base" | "@both").then(|| (id, side.to_string()))
        });
        let Some(entry) = parsed else {
            return Err(format!(
                "`{SCHEME}*` takes one `<id>: @ours` (or @theirs, @base, @both) line per block, or one side for every block; `{}` is neither",
                crate::tools::shorten(line, 60)
            ));
        };
        if plan.iter().any(|(seen, _): &(usize, String)| *seen == entry.0) {
            return Err(format!("`{SCHEME}*` lists conflict #{} twice", entry.0));
        }
        plan.push(entry);
    }
    if plan.is_empty() {
        return Err(format!("`{SCHEME}*` needs at least one `<id>: @side` line, or a single @ours / @theirs / @both"));
    }
    Ok(plan)
}

/// "line 12" or "lines 12–20".
fn range(block: &Block) -> String {
    if block.start == block.end {
        format!("line {}", block.start)
    } else {
        format!("lines {}–{}", block.start, block.end)
    }
}

fn quote(label: &str) -> String {
    if label.is_empty() { "unlabelled".to_string() } else { format!("\"{label}\"") }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFLICTED: &str = "\
use a;

<<<<<<< HEAD
let x = 1;
let y = 2;
=======
let x = 3;
>>>>>>> feature

fn tail() {}
";

    fn registry(dir: &Path) -> (Registry, PathBuf) {
        let file = dir.join("src.rs");
        fs::write(&file, CONFLICTED).unwrap();
        let mut registry = Registry::default();
        registry.reconcile(&file, "src.rs", CONFLICTED);
        (registry, file)
    }

    #[test]
    fn a_read_registers_one_id_per_block() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, file) = registry(dir.path());
        assert_eq!(registry.blocks.len(), 1);
        let block = registry.get(1).unwrap();
        assert_eq!((block.start, block.end), (3, 8));
        assert_eq!(block.ours, ["let x = 1;", "let y = 2;"]);
        assert_eq!(block.theirs, ["let x = 3;"]);
        assert_eq!((block.ours_label.as_str(), block.theirs_label.as_str()), ("HEAD", "feature"));
        assert!(registry.summary(&file).contains("⚠ 1 unresolved conflict in src.rs"));
    }

    #[test]
    fn reconciling_twice_keeps_the_id() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, file) = registry(dir.path());
        registry.reconcile(&file, "src.rs", CONFLICTED);
        assert_eq!(registry.blocks.len(), 1);
        assert_eq!(registry.get(1).map(|block| block.id), Some(1));
    }

    #[test]
    fn a_resolution_replaces_only_the_marker_block() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, file) = registry(dir.path());
        let out = registry.write("1", "let x = 4;").unwrap();
        assert!(out.starts_with("resolved conflict #1 at lines 3–8 in src.rs"), "{out}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "use a;\n\nlet x = 4;\n\nfn tail() {}\n");
        assert!(registry.blocks.is_empty(), "the resolved block stayed registered");
    }

    #[test]
    fn the_shorthands_pick_a_side() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, file) = registry(dir.path());
        registry.write("1", "@theirs").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "use a;\n\nlet x = 3;\n\nfn tail() {}\n");
        assert!(registry.write("1", "@ours").is_err(), "a resolved id must not resolve twice");
        let _ = file;
    }

    #[test]
    fn both_keeps_every_side() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, file) = registry(dir.path());
        registry.write("1", "@both").unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "use a;\n\nlet x = 1;\nlet y = 2;\nlet x = 3;\n\nfn tail() {}\n"
        );
    }

    #[test]
    fn repeated_context_is_dropped_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, file) = registry(dir.path());
        let content = "use a;\n\nlet x = 9;\n\nfn tail() {}\n";
        let out = registry.write("1", content).unwrap();
        assert!(out.contains("dropped 4 replacement line(s)"), "{out}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "use a;\n\nlet x = 9;\n\nfn tail() {}\n");
    }

    #[test]
    fn a_bulk_write_resolves_every_listed_block() {
        let dir = tempfile::tempdir().unwrap();
        let two = CONFLICTED.replace("fn tail", "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> other\n\nfn tail");
        let file = dir.path().join("two.rs");
        fs::write(&file, &two).unwrap();
        let mut registry = Registry::default();
        registry.reconcile(&file, "two.rs", &two);
        assert_eq!(registry.blocks.len(), 2);
        let out = registry.write("*", "1: @ours\n2: @theirs").unwrap();
        assert!(out.starts_with("resolved 2 conflict(s): #1, #2"), "{out}");
        assert!(registry.blocks.is_empty());
        assert_eq!(fs::read_to_string(&file).unwrap(), "use a;\n\nlet x = 1;\nlet y = 2;\n\nb\n\nfn tail() {}\n");
    }

    #[test]
    fn a_scoped_read_shows_one_side() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, _) = registry(dir.path());
        assert_eq!(registry.read("1/theirs").unwrap(), "conflict #1 in src.rs, lines 3–8\nlet x = 3;");
        assert!(registry.read("1/base").unwrap_err().contains("no base section"));
        assert!(registry.read("2").unwrap_err().contains("not registered"));
        assert!(registry.read("*").unwrap_err().contains("write-only"));
    }

    #[test]
    fn a_write_to_a_scoped_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, _) = registry(dir.path());
        assert!(registry.write("1/ours", "@ours").unwrap_err().contains("read-only"));
        assert!(registry.write("7", "@ours").unwrap_err().contains("not registered"));
        assert!(registry.write("*", "1: @sides").unwrap_err().contains("takes one `<id>"));
    }

    #[test]
    fn a_file_rewritten_under_a_block_reports_it_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, file) = registry(dir.path());
        fs::write(&file, "use a;\n").unwrap();
        let out = registry.write("1", "@ours").unwrap_err();
        assert!(out.contains("no longer in src.rs"), "{out}");
    }

    #[test]
    fn crlf_files_keep_their_endings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("crlf.rs");
        let text = CONFLICTED.replace('\n', "\r\n");
        fs::write(&file, &text).unwrap();
        let mut registry = Registry::default();
        registry.reconcile(&file, "crlf.rs", &text);
        registry.write("1", "let x = 4;").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "use a;\r\n\r\nlet x = 4;\r\n\r\nfn tail() {}\r\n");
    }

    #[test]
    fn a_diff3_block_offers_its_base() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("base.rs");
        let text = "<<<<<<< HEAD\na\n||||||| merged common ancestors\nb\n=======\nc\n>>>>>>> x\n";
        fs::write(&file, text).unwrap();
        let mut registry = Registry::default();
        registry.reconcile(&file, "base.rs", text);
        assert_eq!(registry.read("1/base").unwrap().lines().last(), Some("b"));
        registry.write("1", "@base").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "b\n");
    }

    #[test]
    fn an_unterminated_block_is_not_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broken.rs");
        let mut registry = Registry::default();
        registry.reconcile(&file, "broken.rs", "<<<<<<< HEAD\nleft\n");
        assert!(registry.blocks.is_empty());
    }
}
