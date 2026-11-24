use std::fmt::Write as _;
use std::path::Path;

use tokio::fs;

use super::ToolOutcome;
use super::cap::OutputBuffer;
use super::paths::resolve;
use super::registry::{EditArgs, ReadArgs, WriteArgs};

pub(crate) async fn read(cwd: &Path, args: ReadArgs) -> ToolOutcome {
    let path = resolve(cwd, &args.path);
    match fs::read(&path).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            ToolOutcome::ok(number_lines(&text, args.start_line, args.end_line))
        }
        Err(error) => ToolOutcome::error(format!("cannot read {}: {error}", path.display())),
    }
}

pub(crate) async fn write(cwd: &Path, args: WriteArgs) -> ToolOutcome {
    let path = resolve(cwd, &args.path);
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(error) = fs::create_dir_all(parent).await
    {
        return ToolOutcome::error(format!("cannot create {}: {error}", parent.display()));
    }
    match fs::write(&path, &args.content).await {
        Ok(()) => ToolOutcome::ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            path.display()
        )),
        Err(error) => ToolOutcome::error(format!("cannot write {}: {error}", path.display())),
    }
}

pub(crate) async fn edit(cwd: &Path, args: EditArgs) -> ToolOutcome {
    let path = resolve(cwd, &args.path);
    if args.old_string.is_empty() {
        return ToolOutcome::error("old_string must not be empty");
    }
    let text = match fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) => {
            return ToolOutcome::error(format!("cannot read {}: {error}", path.display()));
        }
    };
    match text.matches(&args.old_string).count() {
        0 => ToolOutcome::error(format!(
            "old_string not found in {}; read the file and copy the text exactly",
            path.display()
        )),
        1 => {
            let edited = text.replacen(&args.old_string, &args.new_string, 1);
            match fs::write(&path, edited).await {
                Ok(()) => ToolOutcome::ok(format!("edited {}", path.display())),
                Err(error) => {
                    ToolOutcome::error(format!("cannot write {}: {error}", path.display()))
                }
            }
        }
        count => ToolOutcome::error(format!(
            "old_string appears {count} times in {}; include surrounding lines to make it unique",
            path.display()
        )),
    }
}

/// Numbers the selected lines `  12\ttext`, padding to the width of the last line number. Writes
/// through [`OutputBuffer`], so a huge file costs bounded memory even though its lines are all
/// visited.
fn number_lines(text: &str, start: Option<usize>, end: Option<usize>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return String::new();
    }
    let first = start.unwrap_or(1).clamp(1, total);
    let last = end.unwrap_or(total).clamp(1, total);
    if last < first {
        return format!("[no lines: start_line {first} is after end_line {last}]");
    }
    let width = last.to_string().len();
    let mut out = OutputBuffer::default();
    for (offset, line) in lines[first - 1..last].iter().enumerate() {
        let mut numbered = String::new();
        writeln!(
            numbered,
            "{:>width$}\t{line}",
            first + offset,
            width = width
        )
        .expect("writing to a String cannot fail");
        out.push(&numbered);
    }
    out.finish()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::number_lines;

    #[test]
    fn numbers_every_line_by_default() {
        assert_eq!(number_lines("a\nb\n", None, None), "1\ta\n2\tb\n");
    }

    #[test]
    fn pads_to_the_width_of_the_last_number() {
        let mut text = String::new();
        for n in 1..=12 {
            writeln!(text, "l{n}").expect("writing to a String cannot fail");
        }
        let numbered = number_lines(&text, Some(9), Some(12));
        assert_eq!(numbered, " 9\tl9\n10\tl10\n11\tl11\n12\tl12\n");
    }

    #[test]
    fn selects_a_range_and_clamps_it() {
        assert_eq!(number_lines("a\nb\nc\n", Some(2), Some(99)), "2\tb\n3\tc\n");
        assert_eq!(number_lines("a\nb\nc\n", Some(0), Some(1)), "1\ta\n");
    }

    #[test]
    fn reports_an_inverted_range() {
        assert_eq!(
            number_lines("a\nb\nc\n", Some(3), Some(2)),
            "[no lines: start_line 3 is after end_line 2]"
        );
    }

    #[test]
    fn an_empty_file_numbers_to_nothing() {
        assert_eq!(number_lines("", None, None), "");
    }
}
