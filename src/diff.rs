//! Line diffs between two versions of one text.
//!
//! Myers runs over interned line ids, so the comparison is over integers; the rendered rows keep
//! the original text. A diff past the memory budget falls back to one removal run then one
//! addition run rather than exhausting the process.

use std::collections::HashMap;

/// Rows of both texts past which Myers is skipped: the fallback is a whole-file replace.
const MAX_MYERS_ROWS: usize = 20_000;

/// Cells of trace memory one diff may hold before it falls back. Each cell is an `isize`.
const MAX_TRACE_CELLS: usize = 12_000_000;

/// How one row relates to the two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Context,
    Removed,
    Added,
}

/// One line of the edit script, numbered in the version it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) kind: Kind,
    /// 1-based line in the old text: set for context and removals.
    pub(crate) old: Option<usize>,
    /// 1-based line in the new text: set for context and additions.
    pub(crate) new: Option<usize>,
    pub(crate) text: String,
}

/// Every row of the script, and how many of each kind it holds.
#[derive(Debug, Default)]
pub(crate) struct Diff {
    pub(crate) rows: Vec<Row>,
    pub(crate) added: usize,
    pub(crate) removed: usize,
}

/// The lines of `text`, one trailing empty split dropped: `a\nb\n` is two lines, `""` is none.
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    let mut rows: Vec<&str> = text.split('\n').collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }
    rows
}

/// The line count of `text`, under the same rule `split_lines` uses.
pub(crate) fn line_count(text: &str) -> usize {
    split_lines(text).len()
}

/// The edit script turning `old` into `new`.
pub(crate) fn lines(old: &str, new: &str) -> Diff {
    let old_rows = split_lines(old);
    let new_rows = split_lines(new);
    let mut interner: HashMap<&str, usize> = HashMap::with_capacity(old_rows.len() + new_rows.len());
    let old_ids = intern(&old_rows, &mut interner);
    let new_ids = intern(&new_rows, &mut interner);

    let mut prefix = 0;
    while prefix < old_rows.len() && prefix < new_rows.len() && old_ids.get(prefix) == new_ids.get(prefix) {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_rows.len() - prefix
        && suffix < new_rows.len() - prefix
        && old_ids.get(old_rows.len() - 1 - suffix) == new_ids.get(new_rows.len() - 1 - suffix)
    {
        suffix += 1;
    }
    let old_middle = old_ids.get(prefix..old_rows.len() - suffix).unwrap_or_default();
    let new_middle = new_ids.get(prefix..new_rows.len() - suffix).unwrap_or_default();

    let fallback = || {
        let mut ops = Vec::with_capacity(old_middle.len() + new_middle.len());
        ops.extend((0..old_middle.len()).map(Op::Delete));
        ops.extend((0..new_middle.len()).map(Op::Insert));
        ops
    };
    let middle = if old_middle.len() + new_middle.len() > MAX_MYERS_ROWS {
        fallback()
    } else {
        myers(old_middle, new_middle).unwrap_or_else(fallback)
    };

    let mut ops = Vec::with_capacity(prefix + middle.len() + suffix);
    ops.extend((0..prefix).map(|index| Op::Equal(index, index)));
    for op in middle {
        ops.push(match op {
            Op::Equal(old, new) => Op::Equal(prefix + old, prefix + new),
            Op::Delete(old) => Op::Delete(prefix + old),
            Op::Insert(new) => Op::Insert(prefix + new),
        });
    }
    ops.extend((0..suffix).map(|offset| Op::Equal(old_rows.len() - suffix + offset, new_rows.len() - suffix + offset)));

    let mut diff = Diff::default();
    for op in ops {
        let row = match op {
            Op::Equal(old, new) => Row {
                kind: Kind::Context,
                old: Some(old + 1),
                new: Some(new + 1),
                text: old_rows.get(old).copied().unwrap_or_default().to_string(),
            },
            Op::Delete(old) => {
                diff.removed += 1;
                Row {
                    kind: Kind::Removed,
                    old: Some(old + 1),
                    new: None,
                    text: old_rows.get(old).copied().unwrap_or_default().to_string(),
                }
            }
            Op::Insert(new) => {
                diff.added += 1;
                Row {
                    kind: Kind::Added,
                    old: None,
                    new: Some(new + 1),
                    text: new_rows.get(new).copied().unwrap_or_default().to_string(),
                }
            }
        };
        diff.rows.push(row);
    }
    diff
}

/// One id per distinct line, shared by both texts so equal rows compare equal.
fn intern<'a>(rows: &[&'a str], interner: &mut HashMap<&'a str, usize>) -> Vec<usize> {
    rows.iter()
        .map(|row| {
            let next = interner.len();
            *interner.entry(*row).or_insert(next)
        })
        .collect()
}

/// One move of the edit script, indexed from the start of the compared slices.
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Myers' shortest edit script over interned line ids, or `None` when its trace would pass the
/// memory budget.
fn myers(old: &[usize], new: &[usize]) -> Option<Vec<Op>> {
    let old_len = isize::try_from(old.len()).ok()?;
    let new_len = isize::try_from(new.len()).ok()?;
    let max = usize::try_from(old_len + new_len).ok()?;
    let offset = isize::try_from(max).ok()?;
    let width = max.checked_mul(2)?.checked_add(1)?;
    let mut furthest = vec![0isize; width];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    'search: for distance in 0..=isize::try_from(max).ok()? {
        // The trace is what the backtrack reads: stop before it can outgrow the budget.
        if trace.len().saturating_add(1).saturating_mul(width) > MAX_TRACE_CELLS {
            return None;
        }
        trace.push(furthest.clone());
        let mut diagonal = -distance;
        while diagonal <= distance {
            let mut old_index = if diagonal == -distance
                || (diagonal != distance && at(&furthest, diagonal - 1, offset)? < at(&furthest, diagonal + 1, offset)?)
            {
                at(&furthest, diagonal + 1, offset)?
            } else {
                at(&furthest, diagonal - 1, offset)? + 1
            };
            let mut new_index = old_index - diagonal;
            while old_index < old_len
                && new_index < new_len
                && old.get(usize::try_from(old_index).ok()?)? == new.get(usize::try_from(new_index).ok()?)?
            {
                old_index += 1;
                new_index += 1;
            }
            let slot = usize::try_from(diagonal + offset).ok()?;
            *furthest.get_mut(slot)? = old_index;
            if old_index >= old_len && new_index >= new_len {
                break 'search;
            }
            diagonal += 2;
        }
    }

    let mut ops = Vec::new();
    let mut old_index = old_len;
    let mut new_index = new_len;
    for (level, snapshot) in trace.iter().enumerate().rev() {
        let level = isize::try_from(level).ok()?;
        if level == 0 {
            while old_index > 0 && new_index > 0 {
                old_index -= 1;
                new_index -= 1;
                ops.push(Op::Equal(usize::try_from(old_index).ok()?, usize::try_from(new_index).ok()?));
            }
            break;
        }
        let diagonal = old_index - new_index;
        let previous = if diagonal == -level
            || (diagonal != level && at(snapshot, diagonal - 1, offset)? < at(snapshot, diagonal + 1, offset)?)
        {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let previous_old = at(snapshot, previous, offset)?;
        let previous_new = previous_old - previous;
        while old_index > previous_old && new_index > previous_new {
            old_index -= 1;
            new_index -= 1;
            ops.push(Op::Equal(usize::try_from(old_index).ok()?, usize::try_from(new_index).ok()?));
        }
        if old_index == previous_old {
            ops.push(Op::Insert(usize::try_from(previous_new).ok()?));
        } else {
            ops.push(Op::Delete(usize::try_from(previous_old).ok()?));
        }
        old_index = previous_old;
        new_index = previous_new;
    }
    ops.reverse();
    Some(ops)
}

/// The trace cell at `diagonal`, or `None` when it falls outside the allocated band.
fn at(trace: &[isize], diagonal: isize, offset: isize) -> Option<isize> {
    trace.get(usize::try_from(diagonal + offset).ok()?).copied()
}

/// Rows around each change, with `…` between hunks.
pub(crate) fn render(diff: &Diff, context: usize) -> Vec<String> {
    let changed: Vec<usize> =
        diff.rows.iter().enumerate().filter(|(_, row)| row.kind != Kind::Context).map(|(index, _)| index).collect();
    let Some(first) = changed.first() else { return Vec::new() };
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut index = 0;
    let mut start = first.saturating_sub(context);
    loop {
        let mut end = changed.get(index).copied().unwrap_or(start);
        while index + 1 < changed.len() && changed.get(index + 1).copied().unwrap_or(end + 1) <= end + 1 + context * 2 {
            index += 1;
            end = changed.get(index).copied().unwrap_or(end);
        }
        let end = end.saturating_add(context + 1).min(diff.rows.len());
        if cursor > 0 && start > cursor {
            out.push("…".to_string());
        }
        for row in diff.rows.iter().take(end).skip(start.max(cursor)) {
            out.push(match row.kind {
                Kind::Context => format!(" {}|{}", row.old.unwrap_or(0), row.text),
                Kind::Removed => format!("-{}|{}", row.old.unwrap_or(0), row.text),
                Kind::Added => format!("+{}|{}", row.new.unwrap_or(0), row.text),
            });
        }
        cursor = end;
        index += 1;
        let Some(next) = changed.get(index).copied() else { break };
        start = next.saturating_sub(context);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn kinds(diff: &Diff) -> Vec<Kind> {
        diff.rows.iter().map(|row| row.kind).collect()
    }

    #[test]
    fn identical_texts_have_no_changes() {
        let diff = lines("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(diff.added, 0);
        assert_eq!(diff.removed, 0);
        assert!(kinds(&diff).iter().all(|kind| *kind == Kind::Context));
    }

    #[test]
    fn an_insertion_counts_one_addition() {
        let diff = lines("a\nc\n", "a\nb\nc\n");
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 0);
        assert_eq!(kinds(&diff), [Kind::Context, Kind::Added, Kind::Context]);
        assert_eq!(diff.rows[1].new, Some(2));
        assert_eq!(diff.rows[1].text, "b");
    }

    #[test]
    fn a_replacement_counts_one_of_each() {
        let diff = lines("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        assert_eq!(kinds(&diff), [Kind::Context, Kind::Removed, Kind::Added, Kind::Context]);
        assert_eq!(diff.rows[1].old, Some(2));
        assert_eq!(diff.rows[2].new, Some(2));
    }

    #[test]
    fn a_deletion_counts_one_removal() {
        let diff = lines("a\nb\nc\n", "a\nc\n");
        assert_eq!(diff.added, 0);
        assert_eq!(diff.removed, 1);
        assert_eq!(kinds(&diff), [Kind::Context, Kind::Removed, Kind::Context]);
        assert_eq!(diff.rows[1].old, Some(2));
    }

    #[test]
    fn render_keeps_context_and_marks_a_gap_between_hunks() {
        let mut old = String::new();
        let mut new = String::new();
        for n in 1..=40 {
            let _ = writeln!(old, "{n}");
            match n {
                2 => new.push_str("two\n"),
                38 => new.push_str("thirty-eight\n"),
                _ => {
                    let _ = writeln!(new, "{n}");
                }
            }
        }
        let diff = lines(&old, &new);
        assert_eq!(diff.added, 2);
        assert_eq!(diff.removed, 2);
        let rows = render(&diff, 3);
        assert_eq!(rows.iter().filter(|row| row.as_str() == "…").count(), 1, "{rows:?}");
        assert!(rows.iter().any(|row| row.starts_with("-2|")), "{rows:?}");
        assert!(rows.iter().any(|row| row.starts_with("+2|")), "{rows:?}");
        assert!(rows.iter().any(|row| row == " 1|1"), "{rows:?}");

        let diff = lines("a\nb\n", "a\nB\n");
        let rows = render(&diff, 3);
        assert!(!rows.iter().any(|row| row.as_str() == "…"), "{rows:?}");
        assert_eq!(rows, [" 1|a", "-2|b", "+2|B"]);
    }

    #[test]
    fn a_trailing_newline_does_not_make_an_extra_line() {
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("\n"), 1);
    }
}
