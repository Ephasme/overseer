//! Reading the system clipboard for a paste.
//!
//! The image comes out of the pasteboard through one `osascript` coercion (`the clipboard as
//! «class PNGf»`): the pasteboard satisfies it from a TIFF-only clipboard too, so no PNG encoder is
//! needed here. Text comes from arboard, which spawns nothing, and a clipboard holding neither is
//! reported instead of silently doing nothing.

use crate::config;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The script that writes the clipboard's PNG to the file named by its first argument.
///
/// `on run argv` keeps the path out of the script text: a name with quotes, spaces or accents
/// arrives as an argument, where nothing has to be escaped. A clipboard with no image raises, and
/// the script says so rather than failing the run.
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

/// What a paste found.
#[derive(Debug, PartialEq)]
pub(crate) enum Content {
    /// The clipboard's image, written to this file.
    Image(PathBuf),
    Text(String),
    /// The clipboard holds no image and no text.
    Empty,
    /// The clipboard could not be read: why, in one line.
    Failed(String),
}

/// Reads the clipboard, image first, and writes the image into `dir`.
pub(crate) fn read(dir: &Path) -> Content {
    match image(dir) {
        Content::Empty => match text() {
            Some(text) => Content::Text(text),
            None => Content::Empty,
        },
        found => found,
    }
}

/// Writes the clipboard's PNG into `dir`.
///
/// Only the paste key calls this: it spawns `osascript` and copies every byte of the image, far too
/// slow for an ordinary keystroke.
pub(crate) fn image(dir: &Path) -> Content {
    if let Err(e) = fs::create_dir_all(dir) {
        return Content::Failed(config::io_error(dir.display(), &e));
    }
    let path = target(dir);
    // A file an earlier run left here must not pass for this paste.
    let _ = fs::remove_file(&path);
    let output = match Command::new("osascript").args(["-e", SCRIPT]).arg(&path).output() {
        Ok(output) => output,
        Err(e) => return Content::Failed(format!("osascript: {e}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    classify(output.status.code(), stdout.trim(), stderr.trim(), &path)
}

/// The clipboard's text, when it holds some.
pub(crate) fn text() -> Option<String> {
    let text = arboard::Clipboard::new().ok()?.get_text().ok()?;
    (!text.trim().is_empty()).then_some(text)
}

/// A save target under `dir`: a name no other paste in this process shares, so a stale file from an
/// earlier run can never pass for this one.
fn target(dir: &Path) -> PathBuf {
    dir.join(format!("paste-{}-{}.png", std::process::id(), stamp()))
}

/// What a finished run left behind: an image only when the script said so *and* wrote the file.
fn classify(code: Option<i32>, stdout: &str, stderr: &str, path: &Path) -> Content {
    let written = fs::metadata(path).is_ok_and(|meta| meta.len() > 0);
    if code == Some(0) && stdout == "ok" && written {
        return Content::Image(path.to_path_buf());
    }
    // Half a file is not the image the user asked for.
    let _ = fs::remove_file(path);
    if stdout == "none" {
        return Content::Empty;
    }
    Content::Failed(if !stderr.is_empty() {
        stderr.to_string()
    } else if stdout == "ok" {
        "osascript reported a PNG but wrote no file, so the disk may be full".to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("osascript exited with {code:?} and said nothing")
    })
}

/// A stamp no other paste in this process shares.
fn stamp() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

    #[test]
    fn a_script_that_wrote_the_png_hands_back_its_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = target(dir.path());
        fs::write(&path, PNG).expect("a png");
        assert_eq!(classify(Some(0), "ok", "", &path), Content::Image(path.clone()));
        assert!(path.exists(), "the image stays where the message can point at it");
    }

    #[test]
    fn a_clipboard_without_an_image_is_empty_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = target(dir.path());
        assert_eq!(classify(Some(0), "none", "", &path), Content::Empty);
        assert!(!path.exists());
    }

    #[test]
    fn a_zero_byte_file_is_a_failure_not_an_image() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = target(dir.path());
        fs::write(&path, b"").expect("an empty file");
        let content = classify(Some(0), "ok", "", &path);
        assert!(matches!(content, Content::Failed(_)), "{content:?}");
        assert!(!path.exists(), "half a file is not the image the user asked for");
    }

    #[test]
    fn osascript_errors_are_reported_verbatim() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = target(dir.path());
        assert_eq!(classify(Some(1), "", "not authorized", &path), Content::Failed("not authorized".into()));
        assert_eq!(
            classify(None, "", "", &path),
            Content::Failed("osascript exited with None and said nothing".into())
        );
    }

    /// Local check, never run in CI: put a screenshot on the clipboard, then
    /// `cargo test -- --ignored pasted_pasteboard_image_is_written`.
    #[ignore = "needs a real clipboard"]
    #[test]
    fn pasted_pasteboard_image_is_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Content::Image(path) = image(dir.path()) else { panic!("no image on the clipboard") };
        assert!(fs::read(&path).expect("the png").starts_with(b"\x89PNG"));
    }
}
