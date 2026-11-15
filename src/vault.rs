//! The `.env` credential file behind `vault://`: names only when listed, one value when asked.
//!
//! The file is parsed by `dotenvy`, the same reader `secrets.rs` uses, so a value written here is a
//! value the next start reads.

use crate::config;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Longest value one key may hold.
const MAX_VALUE: usize = 64 * 1024;

/// The keys of the file, in file order; a missing file simply holds none.
fn keys(path: &Path) -> Result<Vec<String>, String> {
    let items = match dotenvy::from_path_iter(path) {
        Ok(items) => items,
        Err(e) if e.not_found() => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut names = Vec::new();
    for item in items {
        let (key, _) = item.map_err(|e| format!("{}: {e}", path.display()))?;
        names.push(key);
    }
    Ok(names)
}

/// The key names only: a value stays where the next start will read it.
pub(crate) fn list(path: &Path) -> String {
    match keys(path) {
        Ok(names) if names.is_empty() => format!("no keys in {}", path.display()),
        Ok(names) => names.join("\n"),
        Err(error) => error,
    }
}

/// One key's value, or why the key names none.
pub(crate) fn read(path: &Path, name: &str) -> Result<String, String> {
    let items = match dotenvy::from_path_iter(path) {
        Ok(items) => Some(items),
        Err(e) if e.not_found() => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let Some(items) = items else {
        return Err(format!("unknown key {name}; keys are none"));
    };
    let mut found = None;
    for item in items {
        let (key, value) = item.map_err(|e| format!("{}: {e}", path.display()))?;
        if key == name {
            found = Some(value);
        }
    }
    found.ok_or_else(|| match keys(path) {
        Ok(names) if names.is_empty() => format!("unknown key {name}; keys are none"),
        Ok(names) => format!("unknown key {name}; keys are {}", names.join(", ")),
        Err(error) => error,
    })
}

/// Sets one key, replacing any line that already names it and keeping every other line as it is.
pub(crate) fn write(path: &Path, name: &str, value: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!("invalid key {name}; use letters, digits and underscore, starting with a letter"));
    }
    if value.contains('\n') || value.contains('\0') {
        return Err(format!("{name}: a value cannot hold a newline or a NUL"));
    }
    if value.len() > MAX_VALUE {
        return Err(format!("{name} is {} bytes; the limit is {MAX_VALUE}", value.len()));
    }
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(config::io_error(path.display(), &e)),
    };
    let mut lines: Vec<String> =
        existing.lines().filter(|line| key_of(line) != Some(name)).map(ToString::to_string).collect();
    lines.push(format!("{name}=\"{}\"", escape(value)));
    let mut text = lines.join("\n");
    text.push('\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| config::io_error(parent.display(), &e))?;
    }
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .and_then(|()| fs::rename(&temp, path));
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(config::io_error(path.display(), &e));
    }
    Ok(format!("stored {name} in {}", path.display()))
}

/// `NAME`: a key a dotenv reader accepts and a shell could export.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The key a line assigns, when it assigns one: comments and blank lines name no key.
fn key_of(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    let key = key.trim();
    (!key.is_empty()).then_some(key)
}

/// `value` in the double quotes `dotenvy` calls weak: `$` is substituted there, so it is escaped too.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_round_trips_through_the_file_and_only_names_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        assert_eq!(write(&path, "TOKEN", "s").unwrap(), format!("stored TOKEN in {}", path.display()));
        assert_eq!(read(&path, "TOKEN").unwrap(), "s");
        assert_eq!(list(&path), "TOKEN");
        // A value carrying both quote kinds and a dollar must come back byte for byte.
        let tricky = "a\"b'c$HOME\\d";
        write(&path, "TRICKY", tricky).unwrap();
        assert_eq!(read(&path, "TRICKY").unwrap(), tricky);
        assert_eq!(list(&path), "TOKEN\nTRICKY");
        write(&path, "TOKEN", "t").unwrap();
        assert_eq!(read(&path, "TOKEN").unwrap(), "t", "a second write replaces the first");
        let listed = list(&path);
        assert!(listed.contains("TOKEN") && listed.contains("TRICKY"), "{listed}");
        assert_eq!(listed.lines().count(), 2, "the replaced line did not get a second copy: {listed}");
        assert!(read(&path, "ABSENT").unwrap_err().starts_with("unknown key ABSENT; keys are "));
        assert!(write(&path, "lower", "x").unwrap_err().contains("invalid key lower"));
        assert!(write(&path, "NEW", "a\nb").unwrap_err().contains("cannot hold a newline"));
        assert!(write(&path, "NEW", &"x".repeat(MAX_VALUE + 1)).unwrap_err().contains("the limit is 65536"));
    }

    #[test]
    fn a_missing_file_reads_as_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        assert_eq!(list(&path), format!("no keys in {}", path.display()));
        assert_eq!(read(&path, "TOKEN").unwrap_err(), "unknown key TOKEN; keys are none");
    }
}
