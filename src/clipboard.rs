use crate::config;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Subdirectory of the system temp dir that holds pasted images.
const DIR: &str = "overseer-attachments";

/// What a paste found on the clipboard.
#[derive(Debug, PartialEq)]
pub(crate) enum Paste {
    /// The clipboard's image, written to this file.
    Image(PathBuf),
    /// The clipboard holds no image.
    Empty,
    /// The clipboard could not be read: why, in one line.
    Failed(String),
}

/// The script that writes the clipboard's PNG to the file named by its first argument.
///
/// `on run argv` keeps the path out of the script text: a name with quotes, spaces or accents arrives
/// as an argument, where nothing has to be escaped. macOS converts a TIFF-only clipboard to PNG on
/// demand, so `«class PNGf»` finds any image copied from an app that offers one at all; a clipboard
/// with no image raises, and the script says so instead of failing the run.
const SCRIPT: &str = "\
on run argv
\tset target to item 1 of argv
\ttry
\t\tset png to the clipboard as «class PNGf»
\ton error
\t\treturn \"none\"
\tend try
\tset f to open for access POSIX file target with write permission
\tset eof f to 0
\twrite png to f
\tclose access f
\treturn \"ok\"
end run";

/// The clipboard's text, when it holds some.
///
/// A terminal hands a TUI its clipboard only as text, in bracketed paste, so this covers the paste
/// that is not an image; arboard reads it without the image support its features would pull in.
pub(crate) fn text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Reads the clipboard's image into a fresh file in the temp dir.
///
/// Only the paste key calls this: it spawns `osascript` and copies every byte of the image, far too
/// slow for a keystroke.
pub(crate) fn image() -> Paste {
    let dir = std::env::temp_dir().join(DIR);
    if let Err(e) = fs::create_dir_all(&dir) {
        return Paste::Failed(config::io_error(dir.display(), &e));
    }
    let path = dir.join(format!("paste-{}-{}.png", std::process::id(), stamp()));
    // A file an earlier run left here must not pass for this paste.
    let _ = fs::remove_file(&path);
    let output = match Command::new("osascript").args(["-e", SCRIPT]).arg(&path).output() {
        Ok(output) => output,
        Err(e) => return Paste::Failed(format!("osascript: {e}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    classify(output.status.code(), stdout.trim(), stderr.trim(), &path)
}

/// Drops a pasted image once nothing needs it.
///
/// Only files this module wrote are removed: any other path belongs to the user.
pub(crate) fn discard(path: &Path) {
    if path.parent().is_some_and(|parent| parent.ends_with(DIR)) {
        let _ = fs::remove_file(path);
    }
}

/// What a finished run left behind: an image only when the script said so *and* wrote the file.
fn classify(code: Option<i32>, stdout: &str, stderr: &str, path: &Path) -> Paste {
    let written = fs::metadata(path).is_ok_and(|meta| meta.len() > 0);
    if code == Some(0) && stdout == "ok" && written {
        return Paste::Image(path.to_path_buf());
    }
    // Half a file is not the image the user asked for.
    let _ = fs::remove_file(path);
    if stdout == "none" {
        return Paste::Empty;
    }
    Paste::Failed(if !stderr.is_empty() {
        stderr.to_string()
    } else if stdout == "ok" {
        "osascript reported a PNG but wrote no file, so the disk may be full".to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("osascript exited with {code:?} and said nothing")
    })
}

/// A name no other paste in this process shares.
fn stamp() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn a_good_run_with_a_written_file_is_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paste.png");
        fs::write(&path, b"\x89PNG\r\n").unwrap();
        assert_eq!(classify(Some(0), "ok", "", &path), Paste::Image(path.clone()));
        assert!(path.exists());
    }

    #[test]
    fn a_clipboard_without_an_image_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paste.png");
        assert_eq!(classify(Some(0), "none", "", &path), Paste::Empty);
        assert!(!path.exists());
    }

    #[test]
    fn a_file_the_script_failed_to_write_is_not_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paste.png");
        fs::write(&path, b"").unwrap();
        let outcome = classify(Some(0), "ok", "", &path);
        assert!(matches!(&outcome, Paste::Failed(why) if why.contains("wrote no file")), "{outcome:?}");
        assert!(!path.exists(), "an empty file is dropped, so it cannot look like a paste");
    }

    #[test]
    fn a_failure_carries_what_the_script_said() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paste.png");
        fs::write(&path, b"\x89PNG\r\n").unwrap();
        let outcome = classify(Some(1), "", "execution error: no permission (-5000)", &path);
        assert!(matches!(&outcome, Paste::Failed(why) if why.contains("no permission")), "{outcome:?}");
        assert!(!path.exists(), "a partial write is dropped");
    }

    #[test]
    fn discard_leaves_files_it_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.png");
        fs::write(&path, b"x").unwrap();
        discard(&path);
        assert!(path.exists(), "a file outside the attachments dir is the user's");
    }

    #[test]
    fn discard_removes_a_pasted_image() {
        let dir = std::env::temp_dir().join(DIR);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("discard-me.png");
        fs::write(&path, b"x").unwrap();
        discard(&path);
        assert!(!path.exists());
    }
}
