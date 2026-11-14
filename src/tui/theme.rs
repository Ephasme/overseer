//! The TUI's palette: one neutral ramp, three hues, and the three status colours.
//!
//! Nothing paints a background, so the terminal's own stays and every token here is a
//! foreground. The ramp is tuned for the dark terminal this tool is run in; body text is
//! near-white rather than the terminal's default because mixing the default with these
//! fixed colours made the chrome look shoehorned in.

use ratatui::style::Color;

/// Anything the reader has to read for minutes.
pub(super) const TEXT: Color = Color::Rgb(0xee, 0xee, 0xee);
/// Secondary reading: state words, tool names, a worker's task.
pub(super) const MUTED: Color = Color::Rgb(0xa0, 0xa0, 0xa0);
/// Chrome: hints, chips, idle dots. Still above 4.5:1, because a chip carries information.
pub(super) const FAINT: Color = Color::Rgb(0x85, 0x85, 0x85);
/// Structural lines. Decorative, so they may sit under AA.
pub(super) const RULE: Color = Color::Rgb(0x2c, 0x31, 0x38);
/// The composer's bar: the one edge that says input goes there.
pub(super) const EDGE: Color = Color::Rgb(0x60, 0x60, 0x60);
/// A selected row's band, barely lifted off the background.
pub(super) const BAND: Color = Color::Rgb(0x1b, 0x20, 0x27);
/// The overseer: its bar, its thinking state, its breadcrumb.
pub(super) const ACCENT: Color = Color::Rgb(0x9d, 0x7c, 0xd8);
/// The user's own words.
pub(super) const PRIMARY: Color = Color::Rgb(0xfa, 0xb2, 0x83);
/// Worker names, and what a worker last said.
pub(super) const INFO: Color = Color::Rgb(0x56, 0xb6, 0xc2);
pub(super) const SUCCESS: Color = Color::Rgb(0x7f, 0xd8, 0x8f);
pub(super) const WARNING: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
pub(super) const ERROR: Color = Color::Rgb(0xe0, 0x6c, 0x75);
