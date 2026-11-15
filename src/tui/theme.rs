//! The TUI's palette: one neutral ramp, three hues, and the three status colours, in three variants.
//!
//! `theme` in `config.toml` picks the variant, and an explicit choice outranks `NO_COLOR`, which
//! only decides what `auto` means. Only three things paint a background: the selected row's band,
//! the user's side of the transcript with the composer under it, and an agent's answer. The rest
//! lets the terminal's own background through: that is why the light ramp exists, to keep the same
//! slots readable when that background is light. `mono` sets every slot to the terminal default,
//! paints no fill and marks the selection with reverse video, so no colour is needed to read it.
//!
//! The dark and light ramps are 24-bit RGB: on a terminal without truecolor the exact contrast
//! ratios below no longer hold, and `mono` is the variant that does not depend on them.

use crate::config::Theme;
use ratatui::style::{Color, Modifier, Style};
use std::{ffi::OsString, sync::OnceLock};

/// Every slot the widgets read. `reversed` says the band is inverse video rather than a fill.
struct Palette {
    text: Color,
    muted: Color,
    faint: Color,
    rule: Color,
    edge: Color,
    band: Color,
    accent: Color,
    primary: Color,
    info: Color,
    /// A worker's answer to its owner.
    reply: Color,
    /// One agent speaking to another: a message call, a lifecycle line.
    message: Color,
    /// The fill behind the user's own words, and under the composer they are typed in.
    you_fill: Color,
    /// The fill behind an agent's answer.
    agent_fill: Color,
    success: Color,
    warning: Color,
    error: Color,
    reversed: bool,
}

/// Resolved once, before the first frame; a frontend that draws without installing gets the dark
/// ramp, which is also what `theme = "auto"` gives on a terminal without `NO_COLOR`.
static CHOSEN: OnceLock<Palette> = OnceLock::new();

fn chosen() -> &'static Palette {
    CHOSEN.get_or_init(dark)
}

/// Fixes the palette for this run, from the config and the environment.
pub(crate) fn install(choice: Theme) {
    let resolved = palette(choice, std::env::var_os("NO_COLOR"));
    debug_assert!(CHOSEN.get().is_none(), "the palette must be installed before the first frame");
    let _ = CHOSEN.set(resolved);
}

/// The config picks the variant; `auto` is the one that reads `NO_COLOR`, and a non-empty value
/// asks for no colour at all.
fn palette(choice: Theme, no_color: Option<OsString>) -> Palette {
    match choice {
        Theme::Auto if no_color.is_some_and(|value| !value.is_empty()) => mono(),
        Theme::Auto | Theme::Dark => dark(),
        Theme::Light => light(),
        Theme::Mono => mono(),
    }
}

/// Tuned for a dark terminal: every text-bearing slot is above 4.5:1 on black; the rule, edge and
/// band are decorative.
fn dark() -> Palette {
    Palette {
        text: Color::Rgb(0xee, 0xee, 0xee),
        muted: Color::Rgb(0xa0, 0xa0, 0xa0),
        faint: Color::Rgb(0x8c, 0x8c, 0x8c),
        rule: Color::Rgb(0x2c, 0x31, 0x38),
        edge: Color::Rgb(0x60, 0x60, 0x60),
        band: Color::Rgb(0x1b, 0x20, 0x27),
        accent: Color::Rgb(0x9d, 0x7c, 0xd8),
        primary: Color::Rgb(0xfa, 0xb2, 0x83),
        info: Color::Rgb(0x56, 0xb6, 0xc2),
        reply: Color::Rgb(0x89, 0xdc, 0xeb),
        message: Color::Rgb(0xff, 0xa3, 0xc7),
        you_fill: Color::Rgb(0x00, 0x00, 0x00),
        agent_fill: Color::Rgb(0x1b, 0x20, 0x27),
        success: Color::Rgb(0x7f, 0xd8, 0x8f),
        warning: Color::Rgb(0xe5, 0xc0, 0x7b),
        error: Color::Rgb(0xe0, 0x6c, 0x75),
        reversed: false,
    }
}

/// The same slots tuned for a light terminal: every slot that carries text is above 4.5:1 on white,
/// and the rule, edge and band stay decorative.
fn light() -> Palette {
    Palette {
        text: Color::Rgb(0x1a, 0x1a, 0x1a),
        muted: Color::Rgb(0x4d, 0x4d, 0x4d),
        faint: Color::Rgb(0x6b, 0x6b, 0x6b),
        rule: Color::Rgb(0xd0, 0xd4, 0xd9),
        edge: Color::Rgb(0x8a, 0x8a, 0x8a),
        band: Color::Rgb(0xe9, 0xed, 0xf2),
        accent: Color::Rgb(0x6d, 0x4b, 0xb8),
        primary: Color::Rgb(0xb4, 0x50, 0x1f),
        info: Color::Rgb(0x0f, 0x6e, 0x78),
        reply: Color::Rgb(0x0a, 0x6b, 0x78),
        message: Color::Rgb(0xa1, 0x2c, 0x72),
        you_fill: Color::Rgb(0xe4, 0xe8, 0xed),
        agent_fill: Color::Rgb(0xf4, 0xf6, 0xf9),
        success: Color::Rgb(0x2c, 0x7a, 0x3c),
        warning: Color::Rgb(0x8a, 0x61, 0x00),
        error: Color::Rgb(0xb3, 0x37, 0x3f),
        reversed: false,
    }
}

/// The terminal's own foreground throughout: structure and glyphs carry the meaning, not hue.
fn mono() -> Palette {
    Palette {
        text: Color::Reset,
        muted: Color::Reset,
        faint: Color::Reset,
        rule: Color::Reset,
        edge: Color::Reset,
        band: Color::Reset,
        accent: Color::Reset,
        primary: Color::Reset,
        info: Color::Reset,
        reply: Color::Reset,
        message: Color::Reset,
        you_fill: Color::Reset,
        agent_fill: Color::Reset,
        success: Color::Reset,
        warning: Color::Reset,
        error: Color::Reset,
        reversed: true,
    }
}

/// Anything the reader has to read for minutes.
pub(super) fn text() -> Color {
    chosen().text
}

/// Secondary reading: state words, tool names, a worker's task.
pub(super) fn muted() -> Color {
    chosen().muted
}

/// Chrome: hints, chips, idle dots. Still above 4.5:1, because a chip carries information.
pub(super) fn faint() -> Color {
    chosen().faint
}

/// Structural lines. Decorative, so they may sit under AA.
pub(super) fn rule() -> Color {
    chosen().rule
}

/// The composer's bar: the one edge that says input goes there.
pub(super) fn edge() -> Color {
    chosen().edge
}

/// A selected row: a fill where colour is available, reverse video where it is not.
pub(super) fn band() -> Style {
    band_style(chosen())
}

fn band_style(palette: &Palette) -> Style {
    if palette.reversed { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new().bg(palette.band) }
}

/// The overseer: its bar, its thinking state, its breadcrumb.
pub(super) fn accent() -> Color {
    chosen().accent
}

/// The user's own words.
pub(super) fn primary() -> Color {
    chosen().primary
}

/// Worker names, and what a worker last said.
pub(super) fn info() -> Color {
    chosen().info
}

/// A worker answering its owner.
pub(super) fn reply() -> Color {
    chosen().reply
}

/// One agent addressing another.
pub(super) fn message() -> Color {
    chosen().message
}

/// The fill behind the user's own words and the composer.
pub(super) fn you_fill() -> Color {
    chosen().you_fill
}

/// The fill behind an agent's answer.
pub(super) fn agent_fill() -> Color {
    chosen().agent_fill
}
pub(super) fn success() -> Color {
    chosen().success
}

pub(super) fn warning() -> Color {
    chosen().warning
}

pub(super) fn error() -> Color {
    chosen().error
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `auto` as the environment leaves it: `Some` is a set `NO_COLOR`, empty or not.
    fn auto(no_color: Option<&str>) -> Palette {
        palette(Theme::Auto, no_color.map(OsString::from))
    }

    #[test]
    fn auto_honours_a_set_no_color_and_ignores_an_empty_one() {
        assert!(auto(Some("1")).reversed, "a non-empty NO_COLOR asks for no colour");
        assert!(!auto(Some("")).reversed, "an empty NO_COLOR is not a request");
        assert!(!auto(None).reversed, "no NO_COLOR means the dark ramp");
    }

    #[test]
    fn an_explicit_theme_outranks_no_color() {
        assert_eq!(palette(Theme::Dark, Some(OsString::from("1"))).text, Color::Rgb(0xee, 0xee, 0xee));
        assert_eq!(palette(Theme::Light, Some(OsString::from("1"))).text, Color::Rgb(0x1a, 0x1a, 0x1a));
        assert!(palette(Theme::Mono, None).reversed);
    }

    #[test]
    fn the_light_ramp_differs_from_the_dark_one_in_every_text_slot() {
        let (dark, light) = (dark(), light());
        for (name, dark, light) in [
            ("text", dark.text, light.text),
            ("muted", dark.muted, light.muted),
            ("faint", dark.faint, light.faint),
            ("accent", dark.accent, light.accent),
            ("primary", dark.primary, light.primary),
            ("info", dark.info, light.info),
            ("reply", dark.reply, light.reply),
            ("message", dark.message, light.message),
            ("success", dark.success, light.success),
            ("warning", dark.warning, light.warning),
            ("error", dark.error, light.error),
        ] {
            assert_ne!(dark, light, "{name} is the same colour in both ramps");
        }
    }

    #[test]
    fn the_two_sides_of_the_transcript_get_two_fills_and_mono_paints_neither() {
        let (dark, light, mono) = (dark(), light(), mono());
        assert_ne!(dark.you_fill, dark.agent_fill, "the user and the agent answer on different fills");
        assert_ne!(light.you_fill, light.agent_fill);
        assert_eq!(mono.you_fill, Color::Reset, "mono paints no fill");
        assert_eq!(mono.agent_fill, Color::Reset);
    }

    #[test]
    fn selection_is_a_fill_where_colour_is_available_and_reverse_video_where_it_is_not() {
        assert_eq!(band_style(&dark()).bg, Some(Color::Rgb(0x1b, 0x20, 0x27)));
        assert_eq!(band_style(&light()).bg, Some(Color::Rgb(0xe9, 0xed, 0xf2)));
        assert!(band_style(&mono()).add_modifier.contains(Modifier::REVERSED), "mono marks the row without a fill");
        assert_eq!(band_style(&mono()).bg, None, "and paints no background");
    }
}
