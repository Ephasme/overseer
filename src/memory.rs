//! Per-project notes an agent leaves for the next session, under the state directory.

use crate::config;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Longest memory file one write creates.
const MAX_BYTES: usize = 1024 * 1024;

/// Longest name one note may take.
const MAX_NAME: usize = 64;

/// The project slug one directory reads as: the canonical path, rooted and flattened.
fn slug(dir: &Path) -> String {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let flattened: String = canonical
        .to_string_lossy()
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if c == '/' {
                '-'
            } else if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect();
    if flattened.is_empty() { "root".to_string() } else { flattened }
}

/// The file one target names: the project note, or a named note beside it.
fn path(root: &Path, dir: &Path, target: &str) -> Result<PathBuf, String> {
    let slug = slug(dir);
    if target.is_empty() || target == "root" {
        return Ok(root.join(format!("{slug}.md")));
    }
    let valid = (1..=MAX_NAME).contains(&target.len())
        && target.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !valid {
        return Err(format!("invalid memory name {target}; use letters, digits, dash, underscore"));
    }
    Ok(root.join(slug).join(format!("{target}.md")))
}

/// The project's memory, or one named note, as `memory://` reads it.
pub(crate) fn read(root: &Path, dir: &Path, target: &str) -> Result<String, String> {
    let file = path(root, dir, target)?;
    match fs::read_to_string(&file) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            Err(format!("no memory for {} yet; write memory:// to start one", dir.display()))
        }
        Err(e) => Err(config::io_error(file.display(), &e)),
    }
}

/// Stores one note. The write lands through a temp file and a rename, so a reader never sees half.
pub(crate) fn write(root: &Path, dir: &Path, target: &str, content: &str) -> Result<String, String> {
    let file = path(root, dir, target)?;
    if content.len() > MAX_BYTES {
        return Err(format!("memory://{target} would be {} bytes; the limit is {MAX_BYTES}", content.len()));
    }
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| config::io_error(parent.display(), &e))?;
    }
    let temp = file.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, content).map_err(|e| config::io_error(temp.display(), &e))?;
    if let Err(e) = fs::rename(&temp, &file) {
        let _ = fs::remove_file(&temp);
        return Err(config::io_error(file.display(), &e));
    }
    Ok(format!("stored {} bytes at memory://{target}", content.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_note_round_trips_and_a_named_one_sits_beside_it() {
        let root = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(read(root.path(), dir.path(), "").unwrap_err().contains("write memory:// to start one"));
        assert_eq!(write(root.path(), dir.path(), "", "x").unwrap(), "stored 1 bytes at memory://");
        assert_eq!(read(root.path(), dir.path(), "").unwrap(), "x");
        assert_eq!(write(root.path(), dir.path(), "todo", "later").unwrap(), "stored 5 bytes at memory://todo");
        assert_eq!(read(root.path(), dir.path(), "todo").unwrap(), "later");
        assert!(
            write(root.path(), dir.path(), "../escape", "x").unwrap_err().contains("invalid memory name"),
            "a separator never reaches the filesystem"
        );
        assert!(write(root.path(), dir.path(), "", &"x".repeat(MAX_BYTES + 1)).unwrap_err().contains("the limit is"));
    }
}
