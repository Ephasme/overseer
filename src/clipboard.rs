//! Reading the system clipboard for a paste.
//!
//! Each step is a fallback of the one before, so a paste is never a silent no-op:
//! 1. arboard's in-process image read (`NSPasteboard` through the `image-data` feature),
//! 2. arboard's text read,
//! 3. `osascript` lifting a PNG out of the pasteboard, only when arboard had nothing at all,
//! 4. [`Content::Empty`], which the caller reports instead of pretending the key worked.
//!
//! `osascript` runs last on purpose: it costs an `AppleScript` round trip and the common paste is
//! text, which step 2 answers without spawning a process. Step 3 covers a pasteboard arboard
//! cannot read (a TIFF it fails to decode, or a clipboard it could not open at all) — the same
//! `AppKit` read `jcode` uses, as `NSPasteboard dataForType:NSPasteboardTypePNG` is what `pi`'s
//! native addon uses in-process.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// What the clipboard held, in a form the paste path can persist.
#[derive(Debug, PartialEq)]
pub(crate) enum Content {
    /// Raw RGBA8 pixels, as arboard hands them back.
    Rgba {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
    /// PNG bytes, as written out by `osascript`.
    Png(Vec<u8>),
    Text(String),
    Empty,
}

/// Reads the clipboard, image before text: a screenshot has no text form, and `get_text` fails on
/// one, which is what made pasting a screenshot do nothing at all.
pub(crate) fn read() -> Content {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        if let Ok(image) = clipboard.get_image() {
            return Content::Rgba { width: image.width, height: image.height, bytes: image.bytes.into_owned() };
        }
        if let Ok(text) = clipboard.get_text()
            && !text.trim().is_empty()
        {
            return Content::Text(text);
        }
    }
    macos_pasteboard_png().map_or(Content::Empty, Content::Png)
}

/// Writes an image [`Content`] as `paste-<millis>.png` under `dir` and returns its path.
pub(crate) fn save(content: &Content, dir: &Path) -> Result<PathBuf> {
    let image_content = matches!(content, Content::Rgba { .. } | Content::Png(_));
    if !image_content {
        bail!("the clipboard held no image");
    }
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let path = dir.join(format!("paste-{stamp}.png"));
    match content {
        Content::Rgba { width, height, bytes } => {
            let buffer = image::RgbaImage::from_raw(rgba_size(*width)?, rgba_size(*height)?, bytes.clone())
                .context("the clipboard image is not RGBA8")?;
            buffer
                .save_with_format(&path, image::ImageFormat::Png)
                .with_context(|| format!("cannot write {}", path.display()))?;
        }
        Content::Png(png) => fs::write(&path, png).with_context(|| format!("cannot write {}", path.display()))?,
        Content::Text(_) | Content::Empty => bail!("the clipboard held no image"),
    }
    Ok(path)
}

/// PNG dimensions are 32-bit: anything wider cannot be written, so say so instead of truncating.
fn rgba_size(value: usize) -> Result<u32> {
    u32::try_from(value).context("the clipboard image is larger than 65535×65535")
}

/// The pasteboard's PNG, through `AppKit`: `none` when it holds no image, so an empty or text-only
/// clipboard pays one `osascript` spawn and nothing else.
#[cfg(target_os = "macos")]
fn macos_pasteboard_png() -> Option<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!("overseer-paste-{}.png", std::process::id()));
    // `dataForType:` mirrors what `pi`'s native addon does in-process; the TIFF branch is for
    // sources that paste a TIFF only. `|properties|` escapes a term AppleScript reserves, which
    // makes the plain `properties:` spelling a parse error.
    let script = format!(
        r#"use framework "AppKit"
set pb to current application's NSPasteboard's generalPasteboard()
set pngData to pb's dataForType:(current application's NSPasteboardTypePNG)
if pngData is missing value then
    set tiffData to pb's dataForType:(current application's NSPasteboardTypeTIFF)
    if tiffData is missing value then return "none"
    set bitmapRep to current application's NSBitmapImageRep's imageRepWithData:tiffData
    set pngData to bitmapRep's representationUsingType:(current application's NSBitmapImageFileTypePNG) |properties|:{{}}
end if
pngData's writeToFile:"{}" atomically:true
return "ok""#,
        tmp.to_string_lossy()
    );
    let output = Command::new("osascript").args(["-l", "AppleScript", "-e", &script]).output().ok()?;
    if String::from_utf8_lossy(&output.stdout).trim() != "ok" {
        return None;
    }
    let png = fs::read(&tmp).ok();
    let _ = fs::remove_file(&tmp);
    png.filter(|bytes| !bytes.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn macos_pasteboard_png() -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn two_by_two() -> Vec<u8> {
        vec![
            255, 0, 0, 255, 0, 255, 0, 255, //
            0, 0, 255, 255, 255, 255, 255, 255,
        ]
    }

    #[test]
    fn rgba_pixels_are_written_as_a_decodable_png() {
        let dir = tempfile::tempdir().expect("temp dir");
        let content = Content::Rgba { width: 2, height: 2, bytes: two_by_two() };
        let path = save(&content, dir.path()).expect("a saved png");
        assert!(path.starts_with(dir.path()));
        let png = fs::read(&path).expect("the png");
        assert_eq!(png.get(..8), Some(b"\x89PNG\r\n\x1a\n".as_slice()));
        assert_eq!(image::load_from_memory(&png).expect("decodable").dimensions(), (2, 2));
    }

    #[test]
    fn png_bytes_are_written_through_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let png =
            fs::read(save(&Content::Rgba { width: 2, height: 2, bytes: two_by_two() }, dir.path()).expect("encoded"))
                .expect("the png");
        let path = save(&Content::Png(png.clone()), dir.path()).expect("a saved png");
        assert_eq!(fs::read(&path).expect("the png"), png);
    }

    /// Local check, never run in CI: put a screenshot on the clipboard, then
    /// `cargo test -- --ignored macos_pasteboard_extracts_a_png`.
    #[cfg(target_os = "macos")]
    #[ignore = "needs a real clipboard"]
    #[test]
    fn macos_pasteboard_extracts_a_png() {
        let png = macos_pasteboard_png().expect("a png on the clipboard");
        assert_eq!(png.get(..8), Some(b"\x89PNG\r\n\x1a\n".as_slice()));
    }

    #[test]
    fn a_clipboard_without_an_image_saves_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(save(&Content::Empty, dir.path()).is_err());
        assert!(save(&Content::Text("hello".into()), dir.path()).is_err());
        assert_eq!(fs::read_dir(dir.path()).expect("readable").count(), 0);
    }
}
