use crate::config;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Subdirectory of the system temp dir that holds pasted images.
const DIR: &str = "overseer-attachments";

/// What a paste found on the clipboard.
#[derive(Debug, PartialEq)]
pub(crate) enum Paste {
    /// The clipboard's text, which belongs in the composer.
    Text(String),
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

/// What the clipboard holds, read in one go.
///
/// The caller runs this off its own thread: the image path spawns `osascript` and copies every byte
/// of the picture, far too slow for the frame that drew the chip.
pub(crate) fn read() -> Paste {
    match text() {
        Some(text) => Paste::Text(text),
        None => image(),
    }
}

/// The clipboard's text, when it holds some.
///
/// A terminal hands a TUI its clipboard only as text, in bracketed paste, so this covers the paste
/// that is not an image; arboard reads it without the image support its features would pull in.
fn text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// How long `osascript` gets to answer before the read is called off.
///
/// One attempt, no retry: a script held up behind a permission prompt must not keep the chip
/// spinning, and ctrl+v dead, for the rest of the session.
const TIMEOUT: Duration = Duration::from_secs(2);

/// How often a read that is waiting looks at the script it spawned.
const WAIT: Duration = Duration::from_millis(20);

/// Reads the clipboard's image into a fresh file in the temp dir.
///
/// Only the paste key calls this, on a thread of its own: it spawns `osascript` and copies every
/// byte of the image, far too slow for a keystroke.
pub(crate) fn image() -> Paste {
    let dir = std::env::temp_dir().join(DIR);
    if let Err(e) = fs::create_dir_all(&dir) {
        return Paste::Failed(config::io_error(dir.display(), &e));
    }
    let path = dir.join(format!("paste-{}-{}.png", std::process::id(), stamp()));
    // A file an earlier run left here must not pass for this paste.
    let _ = fs::remove_file(&path);
    let mut child = match Command::new("osascript")
        .args(["-e", SCRIPT])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Paste::Failed(format!("osascript: {e}")),
    };
    let deadline = Instant::now() + TIMEOUT;
    let answered = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(WAIT),
            Ok(None) => break false,
            Err(e) => return Paste::Failed(format!("osascript: {e}")),
        }
    };
    if !answered {
        let _ = child.kill();
        let _ = child.wait();
        // A half-written file is not the image the user asked for.
        let _ = fs::remove_file(&path);
        return Paste::Failed(format!("osascript gave no answer within {}s", TIMEOUT.as_secs()));
    }
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(e) => return Paste::Failed(format!("osascript: {e}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    classify(output.status.code(), stdout.trim(), stderr.trim(), &path)
}

/// What the attachment chip says about a pasted image.
#[derive(Debug, PartialEq)]
pub(crate) struct Info {
    /// The file's name, without its directory: the chip says what was pasted, not where it went.
    pub(crate) name: String,
    /// Its size on disk, so a screenshot is recognisable before it is sent.
    pub(crate) bytes: u64,
    /// The PNG's pixel size, when its header parses: the closest a terminal gets to a thumbnail.
    pub(crate) pixels: Option<(u32, u32)>,
}

impl Info {
    /// The chip's one line: `paste-1.png · 2560×1600 · 12.9 MB`.
    pub(crate) fn chip(&self) -> String {
        let size = size(self.bytes);
        match self.pixels {
            Some((width, height)) => format!("{} · {width}×{height} · {size}", self.name),
            None => format!("{} · {size}", self.name),
        }
    }
}

/// Reads what the chip shows about `path`. A file that vanished answers with its name alone.
pub(crate) fn info(path: &Path) -> Info {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
    let bytes = fs::metadata(path).map_or(0, |meta| meta.len());
    Info { name, bytes, pixels: pixels(path) }
}

/// A byte count a glance can read: `812 B`, `128.5 kB`, `12.9 MB`.
///
/// Decimal units, integer arithmetic: a size on screen is an aid, and a cast to `f64` for one
/// decimal place is not worth the lint it trips.
fn size(bytes: u64) -> String {
    if bytes < 1_000 {
        return format!("{bytes} B");
    }
    let (divisor, unit) = if bytes < 1_000_000 { (1_000, "kB") } else { (1_000_000, "MB") };
    let tenths = bytes.saturating_mul(10) / divisor;
    format!("{}.{} {unit}", tenths / 10, tenths % 10)
}

/// The PNG's pixel size, read from the 24 bytes that hold the signature and the `IHDR` header.
///
/// The image crate went away with the move to `osascript`; a header this cheap keeps the chip
/// informative without it, and anything that does not parse simply has no size.
fn pixels(path: &Path) -> Option<(u32, u32)> {
    let mut head = [0_u8; 24];
    File::open(path).ok()?.read_exact(&mut head).ok()?;
    if head.get(..8)? != b"\x89PNG\r\n\x1a\n" || head.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(head.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(head.get(20..24)?.try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
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
    fn a_size_is_read_at_a_glance() {
        assert_eq!(size(812), "812 B");
        assert_eq!(size(1_000), "1.0 kB");
        assert_eq!(size(128_500), "128.5 kB");
        assert_eq!(size(12_875_152), "12.8 MB");
        assert_eq!(size(u64::MAX), "1844674407370.9 MB", "a size past any disk still reads");
    }

    #[test]
    fn the_pixel_size_comes_from_the_png_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&2560_u32.to_be_bytes());
        bytes.extend_from_slice(&1600_u32.to_be_bytes());
        fs::write(&path, &bytes).unwrap();
        assert_eq!(pixels(&path), Some((2560, 1600)));
    }

    #[test]
    fn a_file_that_is_not_a_png_has_no_pixel_size() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("short.png");
        fs::write(&short, b"\x89PNG").unwrap();
        assert_eq!(pixels(&short), None, "a file too short to hold a header has no size");
        let other = dir.path().join("other.png");
        fs::write(&other, b"GIF89a\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0").unwrap();
        assert_eq!(pixels(&other), None, "only a PNG header is read");
    }

    #[test]
    fn the_chip_line_names_the_file_its_pixels_and_its_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&1024_u32.to_be_bytes());
        bytes.extend_from_slice(&768_u32.to_be_bytes());
        fs::write(&path, &bytes).unwrap();
        assert_eq!(info(&path).chip(), "shot.png · 1024×768 · 24 B");
    }

    #[test]
    fn a_file_that_is_gone_still_has_a_name_to_show() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vanished.png");
        let info = info(&path);
        assert_eq!(info.name, "vanished.png");
        assert_eq!(info.bytes, 0);
        assert_eq!(info.pixels, None);
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
