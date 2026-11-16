use super::theme;
use crate::approvals;
use crate::clipboard;
use crate::event::UiEvent;
use crate::names;
use crate::prompt;
use crate::slash::{self, RESUME};
use crate::swarm::{AgentEvent, AgentView, Event, Progress, State, Swarm};
use crate::tools;
use crate::version;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tui_markdown::{AlertKind, Options, StyleSheet, from_str_with_options};
use tui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthChar;

/// Frames of the busy spinner.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Shortest call worth timing: below it the number is noise, not information.
const TIMED: f64 = 0.1;

/// Rows the composer grows to before it scrolls.
const COMPOSER_ROWS: u16 = 8;

/// Width the worker panel opens at, its rule included; the user drags the rule from there.
const WORKERS_WIDTH: u16 = 34;

/// Narrowest the worker panel drags to: below it the codename and its state stop fitting.
const WORKERS_MIN: u16 = 20;

/// Columns the transcript keeps whatever the panel is dragged to.
const TRANSCRIPT_MIN: u16 = 40;

/// Rows one wheel notch scrolls.
const WHEEL_LINES: u16 = 3;

/// Redraw interval of the spinner.
const TICK: Duration = Duration::from_millis(120);

/// Key hints, most useful first: a narrow terminal drops the tail rather than the first ones.
const HINTS: [&str; 9] = [
    "enter send",
    "ctrl+j newline",
    "F2 agents",
    "ctrl+o expand",
    "F4 view",
    "ctrl+r rename",
    "F5 mouse",
    "F8 approvals",
    "ctrl+d quit",
];

/// Diff rows a collapsed edit card keeps, before the row that counts what it hid.
const DIFF_ROWS: usize = 10;

/// Said instead of attaching, when the clipboard has neither an image nor text.
const NO_CLIPBOARD: &str = "the clipboard holds no image or text";

/// The head of the attachment chip above the composer.
const ATTACHMENT_LEAD: &str = " ▸ image ";

/// Its remove affordance, kept visible whatever the path's length.
const ATTACHMENT_HINT: &str = " · ctrl+x removes";

/// The same affordance while the chip holds no image to remove.
const ATTACHMENT_DISMISS: &str = " · ctrl+x dismisses";

/// What the chip says while the clipboard is being read.
const ATTACHMENT_READING: &str = "reading the clipboard…";

/// Key hints while an agent's view is open: the way out is on its own header line.
const WORKER_HINTS: [&str; 4] = ["F4 toggle", "ctrl+o expand", "F5 mouse", "ctrl+d quit"];

/// The same line while the rename prompt is open: nothing else takes a key then.
const RENAME_HINTS: [&str; 2] = ["enter accept", "esc cancel"];

/// The same line while an approval prompt is open: it takes every key but the quit ones.
const APPROVAL_HINTS: [&str; 4] = ["y allow once", "a always allow", "n deny", "esc deny"];

/// The rows an approval prompt takes over the composer slot, in a worker view as well as the fleet.
const PROMPT_ROWS: u16 = 3;

/// The composer's frame: one bar down the left, closed by a half block.
const PROMPT_FRAME: border::Set = border::Set {
    top_left: " ",
    top_right: " ",
    bottom_left: "╹",
    bottom_right: " ",
    vertical_left: "┃",
    vertical_right: " ",
    horizontal_top: " ",
    horizontal_bottom: " ",
};

/// A bubble's bar: two columns down its left, on every row of the message.
const BUBBLE_BAR: &str = "▌ ";

/// A panel's edge: one quiet column, no corners, so the panel reads as a region, not a box.
const SIDE_RULE: border::Set = border::Set {
    top_left: " ",
    top_right: " ",
    bottom_left: " ",
    bottom_right: " ",
    vertical_left: "│",
    vertical_right: " ",
    horizontal_top: " ",
    horizontal_bottom: " ",
};

/// What a transcript line is about; decides its gutter and colours.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    /// What an agent was asked: the user's prompt, or the instruction a worker was started with.
    You,
    /// A model's prose: the overseer's or a worker's.
    Model,
    /// The reasoning a model streamed before that prose.
    Thinking,
    /// A tool call, by the overseer or by one of its workers.
    Tool,
    Worker(AgentEvent),
    /// One agent speaking to another, as it lands in its owner's stream.
    Message,
    Notice,
    Error,
    Debug,
}

/// The inline prompt that gives an agent another name.
struct Rename {
    /// The agent being renamed, by the id that never changes.
    id: usize,
    /// How it reads now, so the user sees whose name they replace.
    label: String,
    /// What the user has typed so far.
    text: String,
}

struct ApprovalPrompt {
    /// The agent waiting, by the id that never changes.
    agent: usize,
    /// The gate's request id, what the answer carries back.
    request: u64,
    /// The gated tool's name.
    tool: String,
    /// What the call does, one capped line.
    call: String,
}

struct Entry {
    role: Role,
    /// Tool name, or the worker label.
    title: String,
    /// What the entry says: a message, what a tool call acts on, a worker verb.
    body: String,
    /// The head of a tool result, once the call returns.
    output: Option<String>,
    /// A tool call's verdict, such as `exit 3`.
    status: String,
    /// Result lines beyond the ones shown.
    more: usize,
    failed: bool,
    secs: Option<f64>,
}

impl Entry {
    fn new(role: Role, title: String, body: String) -> Self {
        Self { role, title, body, output: None, status: String::new(), more: 0, failed: false, secs: None }
    }

    /// A tool call's entry.
    fn tool(tool: String, summary: String) -> Self {
        Self::new(Role::Tool, tool, summary)
    }

    fn finish(&mut self, preview: tools::Preview, secs: f64) {
        self.output = Some(preview.head);
        self.status = preview.status;
        self.more = preview.more;
        self.failed = preview.failed;
        self.secs = Some(secs);
    }

    /// Closes a call whose owner stopped before it could report.
    fn abandon(&mut self) {
        self.output = Some("(no result: its agent stopped)".into());
        self.failed = true;
    }

    fn lines(&self, width: u16, spinning: usize, expand: bool) -> Vec<Line<'static>> {
        let width = usize::from(width).max(8);
        match self.role {
            // The user's side: a bubble against the left edge, filled, the bar saying whose it is.
            Role::You => {
                let body = bubble_body(&self.body, width, Style::new().fg(theme::text()));
                bubble(body, Style::new().fg(theme::primary()), theme::you_fill(), false, width)
            }
            // An agent's prose is Markdown, in a bubble of its own pushed to the right edge, so the
            // conversation reads as two columns like any chat.
            Role::Model => {
                let body = markdown(&self.body, "", Style::new(), bubble_text(width));
                bubble(body, Style::new().fg(theme::accent()), theme::agent_fill(), true, width)
            }
            // The reasoning is the same model's: marked as not the answer, and quieter than one.
            Role::Thinking => block(
                "✻ ",
                Style::new().fg(theme::muted()),
                &self.body,
                Style::new().fg(theme::muted()).add_modifier(Modifier::ITALIC),
                width,
            ),
            Role::Tool => self.tool_lines(width, spinning, expand),
            Role::Worker(kind) => {
                let (label, colour) = match kind {
                    AgentEvent::Reply => ("says", theme::reply()),
                    AgentEvent::Finished => ("done", theme::success()),
                    AgentEvent::Failed => ("failed", theme::error()),
                    AgentEvent::Interrupted => ("interrupted", theme::warning()),
                };
                let prefix = format!("[{}] {label} ", self.title);
                if matches!(kind, AgentEvent::Reply | AgentEvent::Finished) {
                    markdown(&self.body, &prefix, Style::new().fg(colour), width)
                } else {
                    block(&prefix, Style::new().fg(colour), &self.body, Style::new().fg(theme::text()), width)
                }
            }
            Role::Message => {
                block("→ ", Style::new().fg(theme::message()), &self.body, Style::new().fg(theme::text()), width)
            }
            Role::Notice => {
                block("· ", Style::new().fg(theme::faint()), &self.body, Style::new().fg(theme::muted()), width)
            }
            Role::Error => {
                block("! ", Style::new().fg(theme::error()), &self.body, Style::new().fg(theme::error()), width)
            }
            Role::Debug => {
                block("… ", Style::new().fg(theme::faint()), &self.body, Style::new().fg(theme::faint()), width)
            }
        }
    }

    /// A tool call: its whole command, and what it returned once expanded.
    fn tool_lines(&self, width: usize, spinning: usize, expand: bool) -> Vec<Line<'static>> {
        if self.title == tools::EDIT && self.output.is_some() && !self.failed {
            return self.diff_lines(width, spinning, expand);
        }
        let (glyph, colour) = if self.output.is_none() {
            (SPINNER.get(spinning % SPINNER.len()).copied().unwrap_or("·"), theme::warning())
        } else if self.failed {
            ("✕", theme::error())
        } else {
            ("✓", theme::success())
        };
        let mut chips: Vec<String> = Vec::new();
        if !self.status.is_empty() {
            chips.push(self.status.clone());
        }
        if let Some(secs) = self.secs.filter(|secs| *secs >= TIMED) {
            chips.push(format!("{secs:.1}s"));
        }
        let chips = if chips.is_empty() { String::new() } else { format!(" · {}", chips.join(" · ")) };
        let verb = verb(&self.title);
        let lead = 1 + text_width(glyph);
        let room = width.saturating_sub(lead + text_width(verb) + 1 + text_width(&chips)).max(12);
        // The whole command at rest: it is what the call does. Any other body, and what the call
        // returned, stay one row until the reader asks for them.
        let mut rows: Vec<String> = Vec::new();
        for source in self.body.split('\n') {
            rows.extend(wrap(source, room));
        }
        if !expand && self.title != tools::BASH {
            rows.truncate(1);
        }
        let pad = " ".repeat(lead + text_width(verb) + 1);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            if index > 0 {
                let mut spans = vec![Span::styled(pad.clone(), Style::new())];
                spans.extend(highlight(&row, Style::new().fg(theme::text())));
                lines.push(Line::from(spans));
                continue;
            }
            let mut spans = vec![
                Span::styled(format!("{glyph} "), Style::new().fg(colour)),
                Span::styled(verb.to_string(), verb_style(&self.title)),
            ];
            if !row.is_empty() {
                spans.push(Span::raw(" "));
                spans.extend(highlight(&row, Style::new().fg(theme::text())));
            }
            spans.push(Span::styled(chips.clone(), Style::new().fg(theme::faint())));
            lines.push(Line::from(spans));
        }
        let Some(output) = &self.output else { return lines };
        if !expand {
            return lines;
        }
        let style = if self.failed { Style::new().fg(theme::error()) } else { Style::new().fg(theme::muted()) };
        let mut rows: Vec<String> = Vec::new();
        for source in output.trim_end().lines() {
            rows.extend(wrap(source, width.saturating_sub(4)));
        }
        let hidden = self.more + rows.len().saturating_sub(tools::PREVIEW_LINES);
        if rows.is_empty() && hidden == 0 {
            rows.push("(no output)".into());
        }
        for row in rows.iter().take(tools::PREVIEW_LINES) {
            let mut spans = vec![Span::styled("  │ ", Style::new().fg(theme::rule()))];
            spans.extend(highlight(row, style));
            lines.push(Line::from(spans));
        }
        if hidden > 0 {
            let plural = if hidden == 1 { "line" } else { "lines" };
            lines.push(Line::from(Span::styled(
                format!("  │ … {hidden} more {plural}"),
                Style::new().fg(theme::faint()),
            )));
        }
        lines
    }

    /// An edit card: the verdict with its counts, then diff rows over an aligned gutter.
    fn diff_lines(&self, width: usize, spinning: usize, expand: bool) -> Vec<Line<'static>> {
        let (glyph, colour) = if self.output.is_none() {
            (SPINNER.get(spinning % SPINNER.len()).copied().unwrap_or("·"), theme::warning())
        } else if self.failed {
            ("✕", theme::error())
        } else {
            ("✓", theme::success())
        };
        let mut header = vec![
            Span::styled(format!("{glyph} "), Style::new().fg(colour)),
            Span::styled("edit".to_string(), Style::new().fg(theme::muted())),
        ];
        if !self.body.is_empty() {
            header.push(Span::raw(" "));
            header.push(Span::styled(self.body.clone(), Style::new().fg(theme::text())));
        }
        if let Some((added, removed)) = self.status.split_once('/') {
            header.push(Span::styled(" ( ".to_string(), Style::new().fg(theme::faint())));
            header.push(Span::styled(added.to_string(), Style::new().fg(theme::success())));
            header.push(Span::styled("/".to_string(), Style::new().fg(theme::faint())));
            header.push(Span::styled(removed.to_string(), Style::new().fg(theme::error())));
            header.push(Span::styled(" )".to_string(), Style::new().fg(theme::faint())));
        } else if !self.status.is_empty() {
            header.push(Span::styled(format!(" · {}", self.status), Style::new().fg(theme::faint())));
        }
        let mut lines = vec![Line::from(header)];
        let Some(output) = &self.output else { return lines };
        let rows = diff_rows(output);
        let numbers = rows.iter().map(|(_, number, _)| number.len()).max().unwrap_or(0);
        let lead = numbers.saturating_add(1);
        let room = width.saturating_sub(lead + 1).max(8);
        let visible = if expand { rows.len() } else { rows.len().min(DIFF_ROWS) };
        for (marker, number, text) in rows.iter().take(visible) {
            let style = match marker {
                '+' => Style::new().fg(theme::success()),
                '-' => Style::new().fg(theme::error()),
                _ => Style::new().fg(theme::muted()),
            };
            let gutter = format!("{marker}{number:>numbers$}");
            for (index, row) in wrap(text, room).into_iter().enumerate() {
                let prefix = if index == 0 { gutter.clone() } else { " ".repeat(lead) };
                lines.push(Line::from(vec![
                    Span::styled(prefix, Style::new().fg(theme::faint())),
                    Span::styled("│", Style::new().fg(theme::rule())),
                    Span::styled(row, style),
                ]));
            }
        }
        let hidden = self.more.saturating_add(rows.len().saturating_sub(visible));
        if hidden > 0 {
            let plural = if hidden == 1 { "line" } else { "lines" };
            let suffix = if expand { "" } else { " (ctrl+o to expand)" };
            lines.push(Line::from(Span::styled(
                format!("… {hidden} more {plural}{suffix}"),
                Style::new().fg(theme::faint()),
            )));
        }
        lines
    }
}

/// Parses `^([ +-])(\d*)\|(.*)$`; a row that does not parse keeps a blank gutter.
fn diff_rows(output: &str) -> Vec<(char, String, String)> {
    output
        .trim_end()
        .lines()
        .map(|line| {
            let mut chars = line.chars();
            let marker = chars.next().unwrap_or(' ');
            let rest = chars.as_str();
            if matches!(marker, ' ' | '+' | '-')
                && let Some((number, text)) = rest.split_once('|')
                && number.chars().all(|char| char.is_ascii_digit())
            {
                return (marker, number.to_string(), text.to_string());
            }
            (' ', String::new(), line.to_string())
        })
        .collect()
}

/// How a tool call reads: a shell command is `$`, the file tools say what they do.
fn verb(tool: &str) -> &str {
    match tool {
        tools::BASH => "$",
        tools::READ_FILE => "read",
        tools::WRITE_FILE => "write",
        tools::EDIT => "edit",
        other => other,
    }
}

/// The verb's colour names the tool's family: a command, a file, an agent, an MCP tool.
fn verb_style(tool: &str) -> Style {
    let colour = match tool {
        prompt::SPAWN_AGENT | prompt::MESSAGE_AGENT | prompt::CONTROL_AGENT | prompt::LIST_AGENTS => theme::message(),
        tools::BASH => theme::warning(),
        tools::READ_FILE | tools::WRITE_FILE => theme::info(),
        other if other.starts_with("mcp__") => theme::accent(),
        _ => theme::muted(),
    };
    Style::new().fg(colour)
}

/// A prefix on the first row, spaces of the same width on the rest, then the wrapped body indented.
fn block(prefix: &str, prefix_style: Style, body: &str, body_style: Style, width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(text_width(prefix)).max(8);
    let pad = " ".repeat(text_width(prefix));
    let mut lines = Vec::new();
    for (line, source) in body.split('\n').enumerate() {
        for (row, text) in wrap(source, room).into_iter().enumerate() {
            let lead = if line == 0 && row == 0 { prefix } else { pad.as_str() };
            let mut spans = vec![Span::styled(lead.to_string(), prefix_style)];
            spans.extend(highlight(&text, body_style));
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// One side of the conversation as a bubble: the bar down every row and the fill under the whole
/// rectangle. It is as wide as its longest row and no wider, so a short message still reads as one;
/// `right` then pushes an agent's answer against the far edge.
fn bubble(body: Vec<Line<'static>>, bar: Style, fill: Color, right: bool, width: usize) -> Vec<Line<'static>> {
    let bar_style = bar.bg(fill);
    let fill_style = Style::new().bg(fill);
    let bar_width = text_width(BUBBLE_BAR);
    let widest = body.iter().map(Line::width).max().unwrap_or(0);
    let room = (widest + bar_width).min(bubble_room(width)).max(bar_width);
    let inset = if right { " ".repeat(width.saturating_sub(room)) } else { String::new() };
    body.into_iter()
        .map(|line| {
            let used = line.width();
            let mut spans = vec![Span::raw(inset.clone()), Span::styled(BUBBLE_BAR.to_string(), bar_style)];
            for span in line.spans {
                spans.push(Span::styled(span.content, fill_style.patch(span.style)));
            }
            spans.push(Span::styled(" ".repeat(room.saturating_sub(bar_width + used)), fill_style));
            Line::from(spans)
        })
        .collect()
}

/// How wide a bubble may grow: most of the transcript, so the gap between the two sides reads.
fn bubble_room(width: usize) -> usize {
    (width.saturating_mul(3) / 4).max(width.min(24))
}

/// The columns a bubble's text may take once its bar is paid for.
fn bubble_text(width: usize) -> usize {
    bubble_room(width).saturating_sub(text_width(BUBBLE_BAR))
}

/// Plain text wrapped to the room a bubble leaves, one row per line the user typed.
fn bubble_body(body: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let room = bubble_text(width);
    let mut lines = Vec::new();
    for source in body.split('\n') {
        for text in wrap(source, room) {
            lines.push(Line::from(highlight(&text, style)));
        }
    }
    lines
}

/// The Markdown renderer's palette: the same slots the transcript already uses.
#[derive(Clone)]
struct MarkdownStyle;

impl StyleSheet for MarkdownStyle {
    fn heading(&self, _level: u8) -> Style {
        Style::new().fg(theme::accent()).add_modifier(Modifier::BOLD)
    }

    fn code(&self) -> Style {
        Style::new().fg(theme::info())
    }

    fn link(&self) -> Style {
        Style::new().fg(theme::reply()).add_modifier(Modifier::UNDERLINED)
    }

    fn blockquote(&self) -> Style {
        Style::new().fg(theme::muted())
    }

    fn heading_meta(&self) -> Style {
        Style::new().fg(theme::faint())
    }

    fn metadata_block(&self) -> Style {
        Style::new().fg(theme::faint())
    }

    fn html(&self) -> Style {
        Style::new().fg(theme::faint())
    }

    fn math_inline(&self) -> Style {
        Style::new().fg(theme::warning())
    }

    fn math_display(&self) -> Style {
        Style::new().fg(theme::warning())
    }

    fn footnote_ref(&self) -> Style {
        Style::new().fg(theme::faint())
    }

    fn footnote_def(&self) -> Style {
        Style::new().fg(theme::faint())
    }

    fn definition_term(&self) -> Style {
        Style::new().fg(theme::text()).add_modifier(Modifier::BOLD)
    }

    fn definition_description(&self) -> Style {
        Style::new().fg(theme::muted())
    }

    fn alert(&self, kind: AlertKind) -> Style {
        let colour = match kind {
            AlertKind::Note => theme::info(),
            AlertKind::Tip => theme::success(),
            AlertKind::Important => theme::accent(),
            AlertKind::Warning => theme::warning(),
            AlertKind::Caution => theme::error(),
        };
        Style::new().fg(colour)
    }

    fn table_header(&self) -> Style {
        Style::new().fg(theme::accent()).add_modifier(Modifier::BOLD)
    }

    fn table_cell(&self) -> Style {
        Style::new().fg(theme::text())
    }

    fn table_border(&self) -> Style {
        Style::new().fg(theme::rule())
    }

    fn list_marker(&self) -> Style {
        Style::new().fg(theme::primary())
    }

    fn image_alt(&self) -> Style {
        Style::new().fg(theme::faint())
    }

    // Headings and fences read by colour and weight; their markers would be noise on a terminal.
    fn heading_marker(&self, _level: u8) -> &'static str {
        ""
    }

    fn code_block_fence(&self) -> &'static str {
        ""
    }
}

/// Gives every prose line an explicit Markdown hard break, leaving fences, indented code and tables
/// untouched: a model's newline is a line break, not a space.
fn hard_breaks(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 32);
    let mut fenced = false;
    for line in body.split('\n') {
        let trimmed = line.trim_start();
        let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if fence {
            fenced = !fenced;
        }
        let literal = fenced || line.starts_with("    ") || line.starts_with('\t');
        if line.is_empty() || literal || trimmed.starts_with('|') {
            out.push_str(line);
        } else {
            out.push_str(line.trim_end());
            out.push_str("  ");
        }
        out.push('\n');
    }
    out.pop();
    out
}

/// Prose as Markdown: rendered, then wrapped to `room`, with `prefix` on the first row only.
///
/// A model writes one newline where it means a line break; `CommonMark` would join the lines into one
/// paragraph, so every prose line gets an explicit hard break before parsing.
fn markdown(body: &str, prefix: &str, prefix_style: Style, width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(text_width(prefix)).max(8);
    let options = Options::new(MarkdownStyle).table_width(u16::try_from(room).unwrap_or(u16::MAX));
    let prepared = hard_breaks(body);
    let text = from_str_with_options(&prepared, &options);
    let pad = " ".repeat(text_width(prefix));
    let mut lines: Vec<Line<'static>> = Vec::new();
    for line in text.lines {
        for row in wrap_styled(&prose_line(line), room) {
            let lead = if lines.is_empty() { prefix } else { pad.as_str() };
            let mut spans = vec![Span::styled(lead.to_string(), prefix_style)];
            spans.extend(row.spans);
            lines.push(Line::from(spans));
        }
    }
    // An empty or still-empty streamed body keeps its gutter, so the transcript does not jump.
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(prefix.to_string(), prefix_style)));
    }
    lines
}

/// A rendered Markdown line as owned spans, in the transcript's text colour, schemes split out.
fn prose_line(line: Line<'_>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let base = line.style;
    for span in line.spans {
        let style = base.patch(span.style);
        let style = if style.fg.is_none() { style.fg(theme::text()) } else { style };
        split_schemes(&mut spans, &span.content, style);
    }
    Line::from(spans)
}

/// One plain row, with every `scheme://…` token split out in the colour of what it reaches.
fn highlight(row: &str, style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    split_schemes(&mut spans, row, style);
    spans
}

/// Splits `text` into spans so an internal URI takes the colour of what it reaches.
fn split_schemes(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    let mut rest = text;
    let mut whole = true;
    while let Some((start, end)) = uri(rest) {
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), style));
        }
        let token = &rest[start..end];
        let scheme = token.split("://").next().unwrap_or_default();
        spans.push(Span::styled(token.to_string(), style.fg(scheme_colour(scheme))));
        rest = &rest[end..];
        whole = false;
    }
    if !rest.is_empty() || whole {
        spans.push(Span::styled(rest.to_string(), style));
    }
}

/// The span of the first internal URI in `row`: its scheme name and the token that follows it.
fn uri(row: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(found) = row[from..].find("://") {
        let separator = from + found;
        let start = row[..separator]
            .rfind(|character: char| !(character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')))
            .map_or(0, |index| index + 1);
        if row[start..separator].starts_with(|character: char| character.is_ascii_alphabetic()) {
            let tail = separator + 3;
            let end = row[tail..]
                .char_indices()
                .find(|(_, character)| {
                    character.is_whitespace() || matches!(character, ')' | ']' | '}' | ',' | ';' | '`' | '"' | '\'')
                })
                .map_or(row.len(), |(index, _)| tail + index);
            return Some((start, end));
        }
        from = separator + 3;
    }
    None
}

/// A scheme's hue: what the reader can tell apart at a glance without reading the token.
fn scheme_colour(scheme: &str) -> Color {
    match scheme {
        "conflict" => theme::warning(),
        "skill" => theme::accent(),
        "agent" | "history" => theme::reply(),
        "mcp" => theme::message(),
        _ => theme::info(),
    }
}

/// Word-wraps one styled line to `width`, keeping each character's style and the line's indentation.
fn wrap_styled(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line.clone()];
    }
    let chars: Vec<(char, Style)> =
        line.spans.iter().flat_map(|span| span.content.chars().map(move |character| (character, span.style))).collect();
    let lead_len = chars.iter().take_while(|(character, _)| character.is_whitespace()).count();
    let (lead, body) = chars.split_at(lead_len);
    let lead = lead.to_vec();
    let lead_width = columns(&lead);
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row = lead.clone();
    let mut used = lead_width;
    let mut index = 0;
    while let Some((character, _)) = body.get(index) {
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        while body.get(index).is_some_and(|(character, _)| !character.is_whitespace()) {
            index += 1;
        }
        let mut word = body.get(start..index).unwrap_or_default();
        loop {
            let spaced = used > lead_width;
            let needed = columns(word);
            if used + usize::from(spaced) + needed <= width {
                if spaced {
                    let style = row.last().map_or_else(Style::default, |(_, style)| *style);
                    row.push((' ', style));
                    used += 1;
                }
                row.extend_from_slice(word);
                used += needed;
                break;
            }
            if spaced {
                rows.push(std::mem::take(&mut row));
                row.clone_from(&lead);
                used = lead_width;
                continue;
            }
            let room = width.saturating_sub(lead_width).max(1);
            let cut = cut_width(word, room);
            let mut head = lead.clone();
            head.extend_from_slice(word.get(..cut).unwrap_or_default());
            rows.push(head);
            word = word.get(cut..).unwrap_or_default();
            if word.is_empty() {
                break;
            }
        }
    }
    if used > lead_width || rows.is_empty() {
        rows.push(row);
    }
    rows.into_iter().map(spans_of).collect()
}

/// Columns a styled run takes on screen.
fn columns(chars: &[(char, Style)]) -> usize {
    chars.iter().map(|(character, _)| char_width(*character)).sum()
}

/// How many characters of `word` fit in `width` columns, always taking at least one.
fn cut_width(word: &[(char, Style)], width: usize) -> usize {
    let mut used = 0;
    let mut cut = 0;
    for (index, (character, _)) in word.iter().enumerate() {
        let next = used + char_width(*character);
        if next > width && cut > 0 {
            break;
        }
        used = next;
        cut = index + 1;
    }
    cut
}

/// Coalesces styled characters into the fewest spans the styles allow.
fn spans_of(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Style> = None;
    for (character, style) in chars {
        if current != Some(style) {
            if let Some(previous) = current {
                spans.push(Span::styled(std::mem::take(&mut text), previous));
            }
            current = Some(style);
        }
        text.push(character);
    }
    if let Some(previous) = current {
        spans.push(Span::styled(text, previous));
    }
    Line::from(spans)
}

/// Word-wraps one line of text to `width` columns, cutting words wider than the line.
///
/// Leading whitespace stays on the first row, so indented tool output keeps its shape.
fn wrap(source: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![source.to_string()];
    }
    let trimmed = source.trim_start();
    // `trimmed` is a suffix of `source`, so the byte offset is a character boundary.
    let lead = source.get(..source.len() - trimmed.len()).unwrap_or_default();
    let lead_width = text_width(lead);
    let mut rows = Vec::new();
    let mut row = lead.to_string();
    let mut used = lead_width;
    for word in trimmed.split_whitespace() {
        let mut rest = word;
        loop {
            let spaced = used > lead_width;
            let space = usize::from(spaced);
            let needed = text_width(rest);
            if used + space + needed <= width {
                if space == 1 {
                    row.push(' ');
                    used += 1;
                }
                row.push_str(rest);
                used += needed;
                break;
            }
            if spaced {
                rows.push(std::mem::take(&mut row));
                row = lead.to_string();
                used = lead_width;
                continue;
            }
            // The word alone is wider than its line: cut it, keeping the indentation.
            let room = width.saturating_sub(lead_width).max(1);
            let (head, tail) = split_at_width(rest, room);
            rows.push(format!("{lead}{head}"));
            rest = tail;
            if rest.is_empty() {
                break;
            }
        }
    }
    if used > lead_width || rows.is_empty() {
        rows.push(row);
    }
    rows
}

/// Splits `word` after the last character that fits in `width`, always taking at least one.
fn split_at_width(word: &str, width: usize) -> (String, &str) {
    let mut used = 0_usize;
    let mut cut = 0_usize;
    for (index, character) in word.char_indices() {
        let next = used + char_width(character);
        if next > width && cut > 0 {
            break;
        }
        used = next;
        cut = index + character.len_utf8();
    }
    let (head, tail) = word.split_at(cut.min(word.len()));
    (head.to_string(), tail)
}

fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// Shortens `text` to `width` columns, marking the cut with an ellipsis.
fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text_width(text) <= width {
        return text.to_string();
    }
    let (head, _) = split_at_width(text, width.saturating_sub(1));
    format!("{head}…")
}

/// Columns a chip's middle keeps before its trailing hint is worth showing.
const CHIP_ROOM: usize = 12;

/// The columns left for a chip's variable middle, and the hint when the line can still hold it.
///
/// A narrow terminal drops the hint rather than the middle: the status line carries the same keys.
fn tail(hint: &str, fixed: usize, width: usize) -> (Option<Span<'static>>, usize) {
    let hint = Span::styled(hint.to_string(), Style::new().fg(theme::faint()));
    let columns = hint.width();
    if width >= fixed + columns + CHIP_ROOM {
        return (Some(hint), width - fixed - columns);
    }
    (None, width.saturating_sub(fixed).max(CHIP_ROOM))
}

/// One view's scrollback: its own entries, the cache of their lines, and its own position.
///
/// Every view keeps its own, so switching views loses neither a buffer nor a scroll position.
struct Stream {
    entries: Vec<Entry>,
    /// Entries flattened at `width`: those before `flattened` are already in `transcript`.
    transcript: Vec<Line<'static>>,
    width: u16,
    /// Rows each entry took, so a rebuild can keep everything before the first dirty entry.
    entry_rows: Vec<usize>,
    flattened: usize,
    dirty_from: usize,
    /// Rows the flattened transcript held, so a rebuild need not re-sum `entry_rows`.
    total_rows: usize,
    /// Follow new lines; off while the reader looks at older ones.
    follow: bool,
    /// Lines scrolled up from the bottom while `follow` is off.
    scroll: u16,
    /// Height and total rows of the last frame this stream drew.
    view: (u16, u16),
}

impl Stream {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            transcript: Vec::new(),
            width: 0,
            entry_rows: Vec::new(),
            flattened: 0,
            dirty_from: 0,
            total_rows: 0,
            follow: true,
            scroll: 0,
            view: (0, 0),
        }
    }

    /// Appends an entry and returns where it landed.
    fn push(&mut self, entry: Entry) -> usize {
        let index = self.entries.len();
        self.entries.push(entry);
        self.entry_rows.push(0);
        self.invalidate(index);
        index
    }

    /// Marks `index` and everything after it as needing a new flatten.
    fn invalidate(&mut self, index: usize) {
        self.dirty_from = self.dirty_from.min(index);
    }

    /// Drops the whole cache: a width change or a verbosity change re-flows every entry.
    fn resize(&mut self) {
        self.dirty_from = 0;
    }

    /// Rebuilds the flattened transcript from the first dirty entry on, keeping what precedes it.
    ///
    /// `entry_rows` counts each entry's own rows plus the gap before it, so truncating at their sum
    /// lands exactly on an entry boundary.
    fn reflatten(&mut self, spinning: usize, expand: bool) {
        let from = self.dirty_from.min(self.entries.len());
        let keep = from.min(self.flattened);
        let rows = if keep == self.flattened {
            self.total_rows
        } else {
            let tail: usize = self.entry_rows.iter().skip(keep).take(self.flattened.saturating_sub(keep)).sum();
            self.total_rows.saturating_sub(tail)
        };
        self.transcript.truncate(rows);
        let mut total = rows;
        for index in keep..self.entries.len() {
            let gap = self.gap_before(index);
            self.transcript.extend(std::iter::repeat_n(Line::default(), gap));
            let lines =
                self.entries.get(index).map_or_else(Vec::new, |entry| entry.lines(self.width, spinning, expand));
            if let Some(slot) = self.entry_rows.get_mut(index) {
                *slot = gap + lines.len();
                total += *slot;
            }
            self.transcript.extend(lines);
        }
        self.total_rows = total;
        self.flattened = self.entries.len();
        self.dirty_from = self.entries.len();
    }

    /// Blank rows before an entry: every message, thought and tool call is a block of its own, and
    /// one blank row keeps them from touching. The reasoning is the one exception, running on from
    /// one thought to the next so a long deliberation stays a single region.
    fn gap_before(&self, index: usize) -> usize {
        let previous = self.entries.get(index.wrapping_sub(1)).map(|entry| &entry.role);
        let current = self.entries.get(index).map(|entry| &entry.role);
        let attached = matches!((previous, current), (Some(Role::Thinking), Some(Role::Thinking)));
        usize::from(index > 0 && !attached)
    }

    /// Flattens what changed, then draws the stream where the reader left it.
    fn draw(&mut self, frame: &mut Frame, area: Rect, spinning: usize, expand: bool) {
        let frame_block = Block::new().padding(Padding::new(2, 2, 1, 0));
        let inner = frame_block.inner(area);
        frame.render_widget(frame_block, area);
        let width = inner.width.max(1);
        if self.width != width {
            self.width = width;
            self.dirty_from = 0;
        }
        if self.dirty_from < self.entries.len() {
            self.reflatten(spinning, expand);
        }
        let total = u16::try_from(self.transcript.len()).unwrap_or(u16::MAX);
        // Reading older lines while new ones arrive: keep the same rows under the cursor.
        if !self.follow && total > self.view.1 {
            self.scroll = self.scroll.saturating_add(total - self.view.1);
        }
        let bottom = total.saturating_sub(inner.height);
        self.scroll = self.scroll.min(bottom);
        let offset = if self.follow { bottom } else { bottom - self.scroll };
        self.view = (inner.height, total);
        frame.render_widget(Paragraph::new(self.transcript.as_slice()).scroll((offset, 0)), inner);
    }

    fn page(&self) -> u16 {
        self.view.0.saturating_sub(2).max(1)
    }

    fn scroll_up(&mut self) {
        let bottom = self.view.1.saturating_sub(self.view.0);
        if bottom > 0 {
            self.follow = false;
            self.scroll = self.scroll.saturating_add(self.page()).min(bottom);
        }
    }

    fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_sub(self.page());
        if self.scroll == 0 {
            self.follow = true;
        }
    }

    fn scroll_top(&mut self) {
        let bottom = self.view.1.saturating_sub(self.view.0);
        if bottom > 0 {
            self.follow = false;
            self.scroll = bottom;
        }
    }

    fn scroll_bottom(&mut self) {
        self.follow = true;
        self.scroll = 0;
    }

    fn wheel(&mut self, up: bool) {
        let bottom = self.view.1.saturating_sub(self.view.0);
        let step = WHEEL_LINES.min(bottom);
        self.scroll = if up {
            if bottom > 0 {
                self.follow = false;
            }
            self.scroll.saturating_add(step).min(bottom)
        } else {
            self.scroll.saturating_sub(step)
        };
        if self.scroll == 0 {
            self.follow = true;
        }
    }
}

/// One worker's view: its own stream, plus the lines that arrived while nobody was looking at it.
struct AgentStream {
    id: usize,
    stream: Stream,
    /// Entries received while this worker's view was closed; cleared when it opens.
    unread: usize,
}

/// One row of the completion popup: what accepting it writes, and how it reads.
enum Candidate {
    /// A slash command, straight from the one table in `slash`.
    Command(&'static slash::Command),
    /// An agent `/resume` can retask.
    Agent { id: usize, state: String, colour: Color, dir: String },
}

impl Candidate {
    /// What accepting the row leaves in the composer, caret at its end.
    fn insertion(&self) -> String {
        match self {
            Self::Command(command) => command.insertion(),
            Self::Agent { id, .. } => format!("{RESUME} {id}"),
        }
    }

    /// The row as the popup draws it: the command and what it does, or the agent and where it sits.
    fn line(&self) -> Line<'static> {
        match self {
            Self::Command(command) => Line::from(vec![
                Span::styled(command.label(), Style::new().fg(theme::text())),
                Span::styled(format!("  {}", command.about), Style::new().fg(theme::muted())),
            ]),
            Self::Agent { id, state, colour, dir } => Line::from(vec![
                Span::styled(id.to_string(), Style::new().fg(theme::info())),
                Span::styled(format!("  {state}"), Style::new().fg(*colour)),
                Span::styled(format!("  {dir}"), Style::new().fg(theme::faint())),
            ]),
        }
    }
}

/// The completion popup: its rows and the one the keys move through.
struct Menu {
    candidates: Vec<Candidate>,
    selected: usize,
}

impl Menu {
    /// The commands `token` names, in the table's order.
    fn commands(token: &str) -> Option<Self> {
        Self::of(slash::matching(token).into_iter().map(Candidate::Command).collect())
    }

    /// The agents an id starting with `partial` names, in the panel's order.
    ///
    /// The list is the live one the panel draws, so an id that no longer exists is never offered.
    fn agents(agents: &[AgentView], partial: &str) -> Option<Self> {
        let candidates = agents.iter().filter(|agent| agent.id.to_string().starts_with(partial)).map(|agent| {
            let (_, colour, state) = worker_state(agent);
            // The state word alone: the panel's own row carries the seconds and the glyph.
            let state = state.split_whitespace().next().unwrap_or_default().to_string();
            Candidate::Agent { id: agent.id, state, colour, dir: shown_dir(&agent.dir) }
        });
        Self::of(candidates.collect())
    }

    /// A menu holding `candidates`, or nothing at all: a token that matches nothing shows no popup.
    fn of(candidates: Vec<Candidate>) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        Some(Self { candidates, selected: 0 })
    }

    /// The highlighted row.
    fn picked(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }

    /// Moves the highlight, wrapping at both ends so a held key never sticks.
    fn step(&mut self, forward: bool) {
        let last = self.candidates.len().saturating_sub(1);
        self.selected = if forward {
            if self.selected >= last { 0 } else { self.selected.saturating_add(1) }
        } else if self.selected == 0 {
            last
        } else {
            self.selected.saturating_sub(1)
        };
    }
}

/// The image the composer carries into the next message, as the chip above it shows it.
enum Attachment {
    /// Ctrl+V landed and the clipboard is being read: the chip says so before the read starts.
    Reading,
    /// The image the paste wrote, with what the chip says about it.
    Ready { path: PathBuf, info: clipboard::Info },
    /// The paste found no image: the chip says why until it is dismissed.
    Failed(String),
}

impl Attachment {
    /// The file this chip holds, when it holds one.
    fn path(&self) -> Option<&Path> {
        match self {
            Self::Ready { path, .. } => Some(path),
            Self::Reading | Self::Failed(_) => None,
        }
    }
}

/// The composer: one bar on the left, room to breathe, and a half block closing the bar. It sits on
/// the same fill as the user's own words, so typing and what was typed share one surface.
fn composer() -> TextArea<'static> {
    let mut area = TextArea::default();
    let fill = Style::new().bg(theme::you_fill());
    // The crate underlines the line holding the caret; this UI does not want underlines.
    area.set_cursor_line_style(Style::new());
    area.set_style(fill);
    area.set_block(
        Block::new()
            .borders(Borders::LEFT | Borders::BOTTOM)
            .border_set(PROMPT_FRAME)
            .border_style(Style::new().fg(theme::edge()))
            .padding(Padding::new(1, 1, 1, 0))
            .style(fill),
    );
    area.set_placeholder_text("Ask agent 1, /resume <id>, /schemes, or exit");
    area.set_wrap_mode(WrapMode::WordOrGlyph);
    area.set_max_rows(COMPOSER_ROWS);
    area
}

#[allow(clippy::struct_excessive_bools)]
pub(super) struct App {
    /// The lead's stream: the overseer's own turns and tool calls, and one line per worker lifecycle event.
    lead: Stream,
    /// One stream per worker, created when the worker joins the roster.
    worker_streams: Vec<AgentStream>,
    composer: TextArea<'static>,
    /// What ctrl+V found: sent with the next message, or a read still in flight, or why there is none.
    attachment: Option<Attachment>,
    /// Id of the read the chip is waiting for; a read whose id is stale lands as nobody's.
    paste: u64,
    model: String,
    history: Vec<String>,
    /// Index into `history` while recalling with ctrl+p and ctrl+n.
    recall: Option<usize>,
    /// The completion popup, while the composer is mid-word on a slash command or an id.
    completion: Option<Menu>,
    /// Where each agent tool call's entry landed: (agent, its call number, entry index).
    calls: Vec<(usize, usize, usize)>,
    /// The entry each agent's streamed output is filling: (agent, entry index in its own stream).
    /// One entry per agent, opened by the first piece of a model call and closed by whatever
    /// arrives that is not streamed output.
    live: Vec<(usize, usize)>,
    workers: Vec<AgentView>,
    worker_panel: bool,
    /// Width the panel wants, rule included: what a drag of its rule sets, and what F2 brings back.
    workers_width: u16,
    /// The column the panel's rule sits on, while the panel is drawn: a drag there resizes it.
    split: Option<u16>,
    /// The area the panel and the transcript share, so a drag can tell how much room is left.
    body_area: Rect,
    /// Whether the pointer is dragging the panel's rule.
    resizing: bool,
    /// Whether the width changed since the frontend last wrote it down.
    width_dirty: bool,
    /// Expanded: every row of what a call acts on, and what it returned. Off is the launch state.
    pub(super) tool_output: bool,
    /// The worker panel's inner area, so a click can tell the panel from the transcript.
    workers_area: Rect,
    /// The rows each worker's block takes in the panel, with that worker's id.
    worker_rows: Vec<(u16, u16, usize)>,
    /// The worker picked in the panel: its row is drawn reversed and its view is a key away.
    selected: Option<usize>,
    /// Whether the picked worker's view replaces the transcript.
    open: bool,
    /// The view's back line, so a click on it returns to the fleet.
    back_area: Rect,
    /// The rename prompt, while it is open.
    rename: Option<Rename>,
    /// Gated calls waiting for the user's decision, oldest first.
    approvals: Vec<ApprovalPrompt>,
    /// The current approval tier, corrected by the first `UiEvent::Approvals`.
    tier: approvals::Tier,
    busy: bool,
    busy_since: Instant,
    last_tick: Instant,
    spinning: usize,
    draining: bool,
    /// Whether the mouse is captured for clicks and the wheel, or handed to the terminal.
    mouse: bool,
    dirty: bool,
    pub(super) quit: bool,
}

impl App {
    pub(super) fn new(model: &str) -> Self {
        Self {
            lead: Stream::new(),
            worker_streams: Vec::new(),
            composer: composer(),
            attachment: None,
            paste: 0,
            rename: None,
            approvals: Vec::new(),
            model: model.to_string(),
            history: Vec::new(),
            tier: approvals::Tier::Ask,
            recall: None,
            completion: None,
            calls: Vec::new(),
            live: Vec::new(),
            workers: Vec::new(),
            worker_panel: true,
            workers_width: WORKERS_WIDTH,
            split: None,
            body_area: Rect::ZERO,
            resizing: false,
            width_dirty: false,
            tool_output: false,
            workers_area: Rect::ZERO,
            worker_rows: Vec::new(),
            selected: None,
            open: false,
            back_area: Rect::ZERO,
            busy: false,
            busy_since: Instant::now(),
            last_tick: Instant::now(),
            spinning: 0,
            draining: false,
            mouse: true,
            dirty: true,
            quit: false,
        }
    }

    pub(super) fn apply(&mut self, event: UiEvent) {
        self.dirty = true;
        match event {
            UiEvent::Notice(text) => self.push(Role::Notice, String::new(), text),
            UiEvent::Answer(text) => self.answer(text),
            UiEvent::Error(text) => self.push(Role::Error, String::new(), text),
            UiEvent::Debug(text) => self.push(Role::Debug, String::new(), text),
            UiEvent::Boundary { id } => self.boundary(id),
            UiEvent::Agent { id, kind, text } => {
                // The id comes first and never changes; the name follows it because the user reads this too.
                let name = format!("agent {id} · {}", self.name(id));
                // The agent's own stream keeps the whole message; the lead gets one lifecycle line.
                let answered = self.close_live(id);
                let first = text.lines().next().unwrap_or_default().trim().to_string();
                let line = if first.is_empty() {
                    format!("{name} {}", kind.verb())
                } else {
                    format!("{name} {}: {first}", kind.verb())
                };
                // A finished turn whose answer already streamed is not written a second time.
                if kind != AgentEvent::Finished || !answered {
                    self.worker_stream_mut(id).push(Entry::new(Role::Worker(kind), name, text));
                    self.note_unread(id);
                }
                // A reply is one agent addressing another; a lifecycle event stays a notice.
                let role = if kind == AgentEvent::Reply { Role::Message } else { Role::Notice };
                self.push(role, String::new(), line);
            }
            UiEvent::AgentDelta { id, text } => self.piece(id, Role::Model, &text),
            UiEvent::AgentThought { id, text } => self.piece(id, Role::Thinking, &text),
            UiEvent::Progress { id, progress } => self.progress(id, progress),
            UiEvent::Instruction { id, text } => {
                // The one entry that opens a worker's stream: what it was told, in its own order.
                self.worker_stream_mut(id).push(Entry::new(Role::You, String::new(), text));
                self.note_unread(id);
            }
            UiEvent::Out { id, text } => {
                // The agent wrote this for the user: it lands in the lead's transcript, as prose.
                let body = format!("agent {id} · {}:\n\n{text}", self.name(id));
                self.push(Role::Model, String::new(), body);
            }
            UiEvent::Agents(agents) => {
                // A child that just joined gets a stream of its own and one line in the lead's. The
                // root already writes into the lead's stream, so it gets neither.
                let joined: Vec<(usize, String)> = agents
                    .iter()
                    .filter(|agent| agent.id != Swarm::ROOT)
                    .filter(|agent| !self.worker_streams.iter().any(|stream| stream.id == agent.id))
                    .map(|agent| (agent.id, agent.dir.display().to_string()))
                    .collect();
                self.workers = agents;
                for (id, dir) in joined {
                    self.worker_stream_mut(id);
                    let line = format!("agent {id} · {} started · {dir}", self.name(id));
                    self.push(Role::Notice, String::new(), line);
                }
                self.close_dead_calls();
            }
            UiEvent::Busy(moving) => {
                self.busy = moving;
                if moving {
                    self.busy_since = Instant::now();
                    // A turn started after ctrl+D: the drain is cancelled.
                    self.draining = false;
                } else {
                    // Nothing is running, so no streamed paragraph is open: a failed turn never
                    // sends an answer and its streamed text stays as the record.
                    self.live.clear();
                }
            }
            UiEvent::Done => self.quit = true,
            UiEvent::ApprovalRequest { agent, request, tool, call } => {
                self.approvals.push(ApprovalPrompt { agent, request, tool, call });
            }
            UiEvent::Approvals(tier) => self.tier = tier,
            UiEvent::ApprovalWithdrawn { request } => self.approvals.retain(|prompt| prompt.request != request),
        }
    }

    /// An agent's tool call: one entry that opens when it starts and closes when it returns.
    fn progress(&mut self, id: usize, progress: Progress) {
        match progress {
            Progress::Started { call, tool, summary } => {
                // The card ends the paragraph above it: what the next model call streams belongs
                // below the card, not glued into the entry it would otherwise still be writing to.
                self.set_live(id, None);
                let index = self.stream_mut(id).push(Entry::tool(tool, summary));
                self.calls.push((id, call, index));
                if id != Swarm::ROOT {
                    self.note_unread(id);
                }
            }
            Progress::Finished { call, secs, preview, .. } => {
                let Some(slot) = self.calls.iter().position(|(worker, seq, _)| *worker == id && *seq == call) else {
                    return;
                };
                let (_, _, index) = self.calls.swap_remove(slot);
                let stream = self.stream_mut(id);
                if let Some(entry) = stream.entries.get_mut(index) {
                    entry.finish(preview, secs);
                }
                stream.invalidate(index);
            }
        }
    }

    /// A worker that stopped mid-call leaves a call that will never return; it closes as failed.
    fn close_dead_calls(&mut self) {
        while let Some(slot) = self.calls.iter().position(|(worker, _, _)| {
            self.workers
                .iter()
                .any(|view| view.id == *worker && matches!(view.state, State::Done | State::Failed | State::Stopped))
        }) {
            let (worker, _, index) = self.calls.swap_remove(slot);
            let stream = self.worker_stream_mut(worker);
            if let Some(entry) = stream.entries.get_mut(index) {
                entry.abandon();
            }
            stream.invalidate(index);
        }
    }

    /// Separates the model calls of one turn with a blank line, in that agent's own stream.
    fn boundary(&mut self, id: usize) {
        let Some(index) = self.live_index(id) else { return };
        let stream = self.stream_mut(id);
        let changed = if let Some(entry) = stream.entries.get_mut(index)
            && !entry.body.is_empty()
            && !entry.body.ends_with("\n\n")
        {
            entry.body.push_str("\n\n");
            true
        } else {
            false
        };
        if changed {
            stream.invalidate(index);
        }
    }

    /// Appends streamed model output to its agent's own stream: the one way a call reaches a view.
    ///
    /// A piece of a different kind opens a new entry, so a stream is strictly the order the call
    /// produced it: reasoning, answer, tool call, then the next call.
    fn piece(&mut self, id: usize, role: Role, text: &str) {
        let opened = match self.live_index(id) {
            Some(index) if self.streamed(id, index).map(|(held, _)| held) == Some(role) => index,
            _ => {
                let index = self.stream_mut(id).push(Entry::new(role, String::new(), String::new()));
                self.set_live(id, Some(index));
                if id != Swarm::ROOT {
                    self.note_unread(id);
                }
                index
            }
        };
        let stream = self.stream_mut(id);
        if let Some(entry) = stream.entries.get_mut(opened) {
            entry.body.push_str(text);
        }
        stream.invalidate(opened);
    }

    /// Closes an agent's streamed entry, saying whether it already carries that agent's answer.
    fn close_live(&mut self, id: usize) -> bool {
        let Some(index) = self.live_index(id) else { return false };
        self.set_live(id, None);
        matches!(self.streamed(id, index), Some((Role::Model, body)) if !body.trim().is_empty())
    }

    /// The turn's final text: it fills the paragraph when nothing streamed, never replaces one.
    fn answer(&mut self, text: String) {
        let live = self.live_index(Swarm::ROOT);
        self.set_live(Swarm::ROOT, None);
        match live {
            Some(index) if self.streamed(Swarm::ROOT, index).map(|(held, _)| held) == Some(Role::Model) => {
                let stream = &mut self.lead;
                if let Some(entry) = stream.entries.get_mut(index)
                    && entry.body.trim().is_empty()
                {
                    entry.body = text;
                }
                stream.invalidate(index);
            }
            _ => self.push(Role::Model, String::new(), text),
        }
    }

    /// The entry one agent's streamed output is filling, if any.
    fn live_index(&self, id: usize) -> Option<usize> {
        self.live.iter().find(|(agent, _)| *agent == id).map(|(_, index)| *index)
    }

    /// Sets or clears the entry one agent's streamed output fills.
    fn set_live(&mut self, id: usize, index: Option<usize>) {
        self.live.retain(|(agent, _)| *agent != id);
        if let Some(index) = index {
            self.live.push((id, index));
        }
    }

    /// One agent's stream, for reading.
    fn stream_ref(&self, id: usize) -> Option<&Stream> {
        if id == Swarm::ROOT {
            return Some(&self.lead);
        }
        self.worker_streams.iter().find(|stream| stream.id == id).map(|worker| &worker.stream)
    }

    /// One entry of one agent's stream: its role and the text it holds.
    fn streamed(&self, id: usize, index: usize) -> Option<(Role, &str)> {
        let entry = self.stream_ref(id)?.entries.get(index)?;
        Some((entry.role, entry.body.as_str()))
    }

    /// An agent's name alone, for a line that already carries its id.
    fn name(&self, id: usize) -> String {
        self.workers
            .iter()
            .find(|agent| agent.id == id)
            .map_or_else(|| names::name(id, None), |agent| agent.name.clone())
    }

    /// How an agent reads here: the label the core sent, or its codename before the roster arrives.
    fn label(&self, id: usize) -> String {
        self.workers
            .iter()
            .find(|agent| agent.id == id)
            .map_or_else(|| names::label(id, None), |agent| agent.label.clone())
    }

    /// The rename prompt, drawn where the composer sits with its frame, so the eye stays put.
    fn rename_widget(&self) -> Paragraph<'static> {
        let Some(rename) = &self.rename else { return Paragraph::default() };
        // The composer's frame has no top border to hang a title on: the prompt names its agent in
        // the line itself.
        let block = Block::new()
            .borders(Borders::LEFT | Borders::BOTTOM)
            .border_set(PROMPT_FRAME)
            .border_style(Style::new().fg(theme::warning()))
            .padding(Padding::new(1, 1, 1, 0))
            .style(Style::new().bg(theme::you_fill()));
        let line = Line::from(vec![
            Span::styled(format!("rename {} ", rename.label), Style::new().fg(theme::warning())),
            Span::styled("› ", Style::new().fg(theme::warning())),
            Span::raw(rename.text.clone()),
            Span::styled("▏", Style::new().fg(theme::warning())),
        ]);
        Paragraph::new(line).block(block)
    }

    /// The approval prompt, drawn with the rename's frame wherever the user is looking.
    ///
    /// The whole call is shown, newlines flattened: approving a multi-line command from its first
    /// line alone would be approving something the user never read.
    fn approval_widget(&self, prompt: &ApprovalPrompt, width: u16) -> Paragraph<'static> {
        let width = usize::from(width.max(24)).saturating_sub(8);
        let block = Block::new()
            .borders(Borders::LEFT | Borders::BOTTOM)
            .border_set(PROMPT_FRAME)
            .border_style(Style::new().fg(theme::warning()))
            .padding(Padding::new(1, 1, 1, 0))
            .style(Style::new().bg(theme::you_fill()));
        let line = Line::from(vec![
            Span::styled(
                format!("approve {} · {} › ", self.label(prompt.agent), prompt.tool),
                Style::new().fg(theme::warning()),
            ),
            Span::styled(truncate(&prompt.call.replace('\n', " ↵ "), width), Style::new().fg(theme::warning())),
            Span::styled("  (y/a/n, esc deny)", Style::new().fg(theme::warning())),
        ]);
        Paragraph::new(line).block(block)
    }

    /// Whether an approval prompt is open: it takes every key but the two quit ones.
    pub(super) fn approvals_open(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// One key while an approval prompt is open. The event it returns is the one the frontend sends.
    ///
    /// A control or alt chord answers nothing: `ctrl+n` is the composer's recall-next, and `ctrl+a`
    /// must not become a session-wide allow by accident.
    pub(super) fn approval_key(&mut self, key: KeyEvent) -> Option<Event> {
        self.dirty = true;
        let request = self.approvals.first()?.request;
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let choice = match key.code {
            KeyCode::Char('y' | 'Y') if plain => Some(approvals::Choice::Once),
            KeyCode::Char('a' | 'A') if plain => Some(approvals::Choice::Always),
            KeyCode::Char('n' | 'N') if plain => Some(approvals::Choice::Deny),
            KeyCode::Esc => Some(approvals::Choice::Deny),
            _ => None,
        };
        choice.map(|choice| {
            self.approvals.remove(0);
            Event::Resolve { request, choice }
        })
    }

    /// Whether the rename prompt is open: every key belongs to it until it closes.
    pub(super) fn renaming(&self) -> bool {
        self.rename.is_some()
    }

    /// Opens the rename prompt on the agent picked in the panel, else on the newest one.
    ///
    /// An open view returns to the fleet first: the prompt sits where the composer does, and the
    /// picked agent stays the one it renames.
    pub(super) fn start_rename(&mut self) {
        self.dirty = true;
        self.open = false;
        let picked = self.selected.filter(|id| self.workers.iter().any(|agent| agent.id == *id));
        let Some(id) = picked.or_else(|| self.workers.last().map(|agent| agent.id)) else {
            self.push(Role::Notice, String::new(), "no agent to rename yet".into());
            return;
        };
        let label = self.label(id);
        self.rename = Some(Rename { id, label, text: String::new() });
    }

    /// One key while the rename prompt is open. The event it returns is the one the frontend sends.
    pub(super) fn rename_key(&mut self, key: KeyEvent) -> Option<Event> {
        self.dirty = true;
        match key.code {
            KeyCode::Esc => {
                self.rename = None;
                None
            }
            KeyCode::Enter => {
                let rename = self.rename.take()?;
                Some(Event::Rename { id: rename.id, name: rename.text.trim().to_string() })
            }
            KeyCode::Backspace => {
                if let Some(rename) = self.rename.as_mut() {
                    rename.text.pop();
                }
                None
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(rename) = self.rename.as_mut() {
                    rename.text.push(character);
                }
                None
            }
            _ => None,
        }
    }

    /// A line in the lead's stream: the only stream the overseer's own events reach.
    fn push(&mut self, role: Role, title: String, body: String) {
        // Anything landing in the transcript closes the streamed paragraph, so no later delta is
        // appended above the line that interrupted it.
        self.set_live(Swarm::ROOT, None);
        self.lead.push(Entry::new(role, title, body));
    }

    /// The stream of one worker, created on first use so no event is ever dropped.
    fn worker_stream_mut(&mut self, id: usize) -> &mut Stream {
        let slot = if let Some(slot) = self.worker_streams.iter().position(|stream| stream.id == id) {
            slot
        } else {
            self.worker_streams.push(AgentStream { id, stream: Stream::new(), unread: 0 });
            self.worker_streams.len().saturating_sub(1)
        };
        if let Some(worker) = self.worker_streams.get_mut(slot) {
            &mut worker.stream
        } else {
            debug_assert!(false, "the slot was just obtained from this vector");
            &mut self.lead
        }
    }

    /// The stream an agent's events belong to: the transcript for the root, its own buffer otherwise.
    fn stream_mut(&mut self, id: usize) -> &mut Stream {
        if id == Swarm::ROOT { &mut self.lead } else { self.worker_stream_mut(id) }
    }

    /// Counts a line a worker's stream received while its view was closed.
    fn note_unread(&mut self, id: usize) {
        if self.viewed() == Some(id) {
            return;
        }
        if let Some(worker) = self.worker_streams.iter_mut().find(|stream| stream.id == id) {
            worker.unread = worker.unread.saturating_add(1);
        }
    }

    /// Lines a worker received since its view was last opened.
    fn unread_of(&self, id: usize) -> usize {
        self.worker_streams.iter().find(|stream| stream.id == id).map_or(0, |stream| stream.unread)
    }

    /// Whether a frame is due; clears the flag.
    pub(super) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// A resize changes the transcript width, so the whole cache is rebuilt on the next draw.
    pub(super) fn resize(&mut self) {
        self.lead.resize();
        for worker in &mut self.worker_streams {
            worker.stream.resize();
        }
        self.dirty = true;
    }

    /// Advances the spinner, which turns for a thinking overseer and for a tool call in flight.
    pub(super) fn tick(&mut self) {
        let running = self.running_tool();
        if (!self.busy && running.is_none() && !self.reading()) || self.last_tick.elapsed() < TICK {
            return;
        }
        self.last_tick = Instant::now();
        self.spinning = self.spinning.wrapping_add(1);
        self.dirty = true;
        self.invalidate_running_tools();
    }

    /// Invalidates every in-flight tool entry, so every stream with a call animates its spinner.
    fn invalidate_running_tools(&mut self) {
        let lead_indices: Vec<usize> = self
            .lead
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.role == Role::Tool && entry.output.is_none())
            .map(|(index, _)| index)
            .collect();
        for index in lead_indices {
            self.lead.invalidate(index);
        }
        for worker in &mut self.worker_streams {
            let indices: Vec<usize> = worker
                .stream
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.role == Role::Tool && entry.output.is_none())
                .map(|(index, _)| index)
                .collect();
            for index in indices {
                worker.stream.invalidate(index);
            }
        }
    }

    /// The first tool call still in flight, with its owner: `None` is the lead's own call.
    fn running_tool(&self) -> Option<(Option<usize>, usize)> {
        let lead = self.lead.entries.iter().position(|entry| entry.role == Role::Tool && entry.output.is_none());
        if let Some(index) = lead {
            return Some((None, index));
        }
        for worker in &self.worker_streams {
            let index =
                worker.stream.entries.iter().position(|entry| entry.role == Role::Tool && entry.output.is_none());
            if let Some(index) = index {
                return Some((Some(worker.id), index));
            }
        }
        None
    }

    pub(super) fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let view = self.in_worker_view();
        let attached = u16::from(self.attachment.is_some() && !view);
        let rows = if view {
            // An approval prompt is drawn over the composer slot in a worker view too: its keys
            // are taken there, so it must be visible there.
            if self.approvals.is_empty() { 0 } else { PROMPT_ROWS }
        } else {
            let rows = self.composer.measure(area.width).preferred_rows;
            rows.clamp(3, area.height.saturating_sub(4 + attached).max(3))
        };
        let [body, chip, input, status] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(attached),
            Constraint::Length(rows),
            Constraint::Length(1),
        ])
        .areas(area);
        if view {
            self.render_worker_view(frame, body);
            if let Some(first) = self.approvals.first() {
                let prompt = self.approval_widget(first, input.width);
                frame.render_widget(prompt, input);
            }
        } else {
            self.body_area = body;
            let transcript = if let Some(width) = self.panel_width(body.width) {
                let [left, right] =
                    Layout::horizontal([Constraint::Min(TRANSCRIPT_MIN), Constraint::Length(width)]).areas(body);
                self.render_workers(frame, right);
                left
            } else {
                self.workers_area = Rect::ZERO;
                self.worker_rows.clear();
                self.split = None;
                body
            };
            self.render_transcript(frame, transcript);
            self.render_attachment(frame, chip);
            if let Some(first) = self.approvals.first() {
                let prompt = self.approval_widget(first, input.width);
                frame.render_widget(prompt, input);
            } else if self.rename.is_some() {
                let prompt = self.rename_widget();
                frame.render_widget(prompt, input);
            } else {
                frame.render_widget(&self.composer, input);
                self.render_composer_meta(frame, input);
                self.render_completion(frame, input);
            }
        }
        self.render_status(frame, status);
    }

    fn render_transcript(&mut self, frame: &mut Frame, area: Rect) {
        let (spinning, expand) = (self.spinning, self.tool_output);
        self.lead.draw(frame, area, spinning, expand);
    }

    /// The tree's roster: one quiet rule, then a block per agent, the picked one banded.
    fn render_workers(&mut self, frame: &mut Frame, area: Rect) {
        let rule = Block::new()
            .borders(Borders::LEFT)
            .border_set(SIDE_RULE)
            .border_style(Style::new().fg(theme::rule()))
            .padding(Padding::new(1, 0, 1, 0));
        let inner = rule.inner(area);
        frame.render_widget(rule, area);
        self.workers_area = inner;
        self.split = Some(area.x);
        self.worker_rows.clear();
        let mut head = vec![
            Span::styled("agents", Style::new().fg(theme::muted())),
            Span::styled(format!(" {}", self.workers.len()), Style::new().fg(theme::faint())),
        ];
        if self.workers.is_empty() {
            head.push(Span::styled("  none yet", Style::new().fg(theme::faint())));
            frame.render_widget(Paragraph::new(Line::from(head)), Rect { height: 1, ..inner });
            return;
        }
        frame.render_widget(Paragraph::new(Line::from(head)), Rect { height: 1, ..inner });
        let mut top = inner.y.saturating_add(1);
        let mut hidden = false;
        for worker in &self.workers {
            let (glyph, colour, state) = worker_state(worker);
            let height = 3_u16;
            let used = top.saturating_sub(inner.y);
            if used.saturating_add(height) > inner.height {
                hidden = true;
                break;
            }
            let unread = self.unread_of(worker.id);
            let indent = "  ".repeat(worker.depth);
            // The name gives way to the state: a row that loses the state says less. The counter sits
            // on the directory line, where the short label leaves room for it.
            let reserved = text_width(&indent) + text_width(glyph) + 1 + text_width(&state);
            let label = truncate(&worker.label, usize::from(inner.width).saturating_sub(reserved));
            let marker = if unread == 0 { String::new() } else { format!(" · {unread} new") };
            let lines = vec![
                Line::from(vec![
                    Span::styled(format!("{indent}{glyph} "), Style::new().fg(colour)),
                    Span::styled(
                        label,
                        Style::new().fg(if self.selected == Some(worker.id) { theme::text() } else { theme::info() }),
                    ),
                    Span::styled(
                        if worker.profile.is_empty() { String::new() } else { format!(" {}", worker.profile) },
                        Style::new().fg(theme::faint()),
                    ),
                    Span::styled(format!(" {state}"), Style::new().fg(theme::muted())),
                ]),
                Line::from(vec![
                    Span::styled(short_dir(&worker.dir), Style::new().fg(theme::faint())),
                    Span::styled(marker, Style::new().fg(theme::warning())),
                ]),
                Line::default(),
            ];
            let band = if self.selected == Some(worker.id) { theme::band() } else { Style::new() };
            let block = Rect { x: inner.x, y: top, width: inner.width, height };
            frame.render_widget(Paragraph::new(lines).style(band), block);
            self.worker_rows.push((top, top.saturating_add(height), worker.id));
            top = top.saturating_add(height);
        }
        if hidden {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("…", Style::new().fg(theme::faint())))),
                Rect { x: inner.x, y: top, width: inner.width, height: 1 },
            );
        }
    }

    /// The pending image, one compact chip above the composer: what was pasted, what it weighs, and
    /// whether it is still being read. The chip never wraps, and a long name is cut rather than the
    /// verdict or the way out being pushed off the line.
    fn render_attachment(&self, frame: &mut Frame, area: Rect) {
        let Some(attachment) = &self.attachment else { return };
        let lead = Span::styled(ATTACHMENT_LEAD, Style::new().fg(theme::primary()));
        // The arrow is three bytes and one column: the chip's arithmetic is in columns.
        let lead_width = text_width(ATTACHMENT_LEAD);
        // The chip pays for its padding before it places a single column of text.
        let width = usize::from(area.width).saturating_sub(4);
        let line = match attachment {
            Attachment::Reading => {
                let glyph = SPINNER.get(self.spinning % SPINNER.len()).copied().unwrap_or("⠋");
                let room = width.saturating_sub(lead_width);
                let message = truncate(&format!("{glyph} {ATTACHMENT_READING}"), room);
                Line::from(vec![lead, Span::styled(message, Style::new().fg(theme::warning()))])
            }
            Attachment::Ready { info, .. } => {
                let status = Span::styled(" · ✓ attached", Style::new().fg(theme::success()));
                let (hint, room) = tail(ATTACHMENT_HINT, lead_width + status.width(), width);
                let mut spans =
                    vec![lead, Span::styled(truncate(&info.chip(), room), Style::new().fg(theme::text())), status];
                spans.extend(hint);
                Line::from(spans)
            }
            Attachment::Failed(why) => {
                let (hint, room) = tail(ATTACHMENT_DISMISS, lead_width + 2, width);
                let verdict = Span::styled(format!("✗ {}", truncate(why, room)), Style::new().fg(theme::error()));
                let mut spans = vec![lead, verdict];
                spans.extend(hint);
                Line::from(spans)
            }
        };
        frame.render_widget(Paragraph::new(line).block(Block::new().padding(Padding::horizontal(2))), area);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![Span::raw("  ")];
        if self.busy {
            let glyph = SPINNER.get(self.spinning % SPINNER.len()).copied().unwrap_or("⠋");
            spans.push(Span::styled(format!("{glyph} thinking "), Style::new().fg(theme::muted())));
            let secs = self.busy_since.elapsed().as_secs_f32();
            spans.push(Span::styled(format!("{secs:.0}s"), Style::new().fg(theme::muted())));
        } else if self.draining {
            spans.push(Span::styled("draining, quits when agents idle", Style::new().fg(theme::warning())));
        } else {
            spans.push(Span::styled("● idle", Style::new().fg(theme::muted())));
        }
        let working = self.workers.iter().filter(|w| w.state == State::Working).count();
        let paused = self.workers.iter().filter(|w| w.state == State::Paused).count();
        let idle = self.workers.len().saturating_sub(working + paused);
        let paused = if paused == 0 { String::new() } else { format!("{paused} paused · ") };
        spans
            .push(Span::styled(format!(" · {working} working · {paused}{idle} idle"), Style::new().fg(theme::muted())));
        spans.push(Span::styled(format!(" · {}", self.tier.name()), Style::new().fg(theme::muted())));
        if let Some(scroll) = self.scrolled() {
            spans.push(Span::styled(format!(" · ↑{scroll} lines"), Style::new().fg(theme::warning())));
        }
        let used = Line::from(spans.clone()).width();
        let hints = if !self.approvals.is_empty() {
            APPROVAL_HINTS.as_slice()
        } else if self.renaming() {
            RENAME_HINTS.as_slice()
        } else if self.in_worker_view() {
            WORKER_HINTS.as_slice()
        } else {
            HINTS.as_slice()
        };
        // Two columns of inset on each side, the same as everything else on screen.
        let room = usize::from(area.width).saturating_sub(used + 2);
        let hint = fit_hints(hints, room);
        let filler = usize::from(area.width).saturating_sub(used + text_width(&hint) + 2);
        spans.push(Span::raw(" ".repeat(filler.max(1))));
        if !hint.is_empty() {
            spans.push(Span::styled(hint, Style::new().fg(theme::faint())));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The composer's cap says who answers and which build: the model, then the version.
    fn render_composer_meta(&self, frame: &mut Frame, area: Rect) {
        if area.height < 2 {
            return;
        }
        let cap = Rect {
            x: area.x.saturating_add(2),
            y: area.y.saturating_add(area.height.saturating_sub(1)),
            width: area.width.saturating_sub(2),
            height: 1,
        };
        let model = truncate(&self.model, 32);
        let build = version::VERSION;
        let width = usize::from(cap.width);
        let mut spans = vec![Span::styled(model.clone(), Style::new().fg(theme::muted()))];
        // Two trailing columns keep the version off the right edge; the rest is filler.
        let used = text_width(&model) + text_width(build) + 2;
        if width > used {
            spans.push(Span::raw(" ".repeat(width - used)));
            spans.push(Span::styled(build, Style::new().fg(theme::faint())));
            spans.push(Span::raw("  "));
        }
        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line), cap);
    }

    /// The completion menu, hanging off the composer: its rows sit just above it and its text
    /// starts in the column the composer's own text does. The highlighted row wears the band the
    /// panels mark a picked row with, so it reads as picked in all three palettes.
    fn render_completion(&self, frame: &mut Frame, composer: Rect) {
        let Some(menu) = &self.completion else { return };
        // Rows the space above the composer cannot hold are simply not drawn: the frame keeps its
        // shape on a terminal too short for the whole menu.
        let height = u16::try_from(menu.candidates.len()).unwrap_or(u16::MAX).min(composer.y);
        if height == 0 {
            return;
        }
        let area = Rect {
            x: composer.x,
            y: composer.y.saturating_sub(height),
            width: composer.width.saturating_sub(2),
            height,
        };
        // The transcript runs underneath: the popup's rows are its own, not the entry behind it.
        frame.render_widget(Clear, area);
        for (index, candidate) in menu.candidates.iter().enumerate() {
            let Ok(offset) = u16::try_from(index) else { return };
            let band = if index == menu.selected { theme::band() } else { Style::new() };
            let row = Rect { y: area.y.saturating_add(offset), height: 1, ..area };
            let line = Paragraph::new(candidate.line()).style(band).block(Block::new().padding(Padding::horizontal(2)));
            frame.render_widget(line, row);
        }
    }

    /// Whether a worker view is open: the picked worker is still in the roster and the view is not closed.
    pub(super) fn in_worker_view(&self) -> bool {
        self.viewed().is_some()
    }

    /// The agent the stop key would end: the open worker's, while it still has work. The root is
    /// never one: it is the user's only voice, and nothing in the program restarts it.
    pub(super) fn stop_target(&self) -> Option<usize> {
        let id = self.viewed().filter(|id| *id != Swarm::ROOT)?;
        let worker = self.workers.iter().find(|worker| worker.id == id)?;
        matches!(worker.state, State::Working | State::Paused).then_some(id)
    }

    /// Acknowledges the stop key before the core reports it: a model call already in flight cannot
    /// be cut mid-answer, and the wait must not read as a dead key.
    pub(super) fn stopping(&mut self, id: usize) {
        self.active().push(Entry::new(Role::Notice, String::new(), format!("stopping agent {id}…")));
        self.dirty = true;
    }

    /// The worker whose view is open; the lead's stream is the view otherwise.
    fn viewed(&self) -> Option<usize> {
        if !self.open {
            return None;
        }
        let id = self.selected?;
        self.workers.iter().any(|worker| worker.id == id).then_some(id)
    }

    /// The worker the view is about.
    fn focused(&self) -> Option<&AgentView> {
        let id = self.viewed()?;
        self.workers.iter().find(|worker| worker.id == id)
    }

    /// The stream the keys and the wheel act on: the open worker's, else the lead's.
    fn active(&mut self) -> &mut Stream {
        match self.viewed() {
            Some(id) => self.worker_stream_mut(id),
            None => &mut self.lead,
        }
    }

    /// How far the active stream is scrolled up, when it is not following new lines.
    fn scrolled(&self) -> Option<u16> {
        let stream = match self.viewed() {
            Some(id) => self.worker_streams.iter().find(|stream| stream.id == id).map(|worker| &worker.stream),
            None => Some(&self.lead),
        }?;
        (!stream.follow).then_some(stream.scroll)
    }

    /// Opens one worker's view and clears what it collected while nobody was looking.
    fn open_worker(&mut self, id: usize) {
        self.selected = Some(id);
        self.open = true;
        if let Some(worker) = self.worker_streams.iter_mut().find(|stream| stream.id == id) {
            worker.unread = 0;
        }
        self.dirty = true;
    }

    /// A left click: a worker's block opens its view, the view's back line returns to the fleet.
    pub(super) fn click(&mut self, column: u16, row: u16) {
        let point = Position::new(column, row);
        if self.in_worker_view() {
            if self.back_area.contains(point) {
                self.leave_worker();
            }
            return;
        }
        // The rule between the two panes is a handle: pressing it starts a drag, not a pick.
        if self.split == Some(column) {
            self.resizing = true;
            return;
        }
        if !self.workers_area.contains(point) {
            return;
        }
        let picked =
            self.worker_rows.iter().find(|(top, bottom, _)| (*top..*bottom).contains(&row)).map(|(_, _, id)| *id);
        let Some(id) = picked else { return };
        self.open_worker(id);
    }

    /// F4: open the picked worker's view, the first worker's when none is picked, or close it.
    pub(super) fn toggle_worker_view(&mut self) {
        if self.in_worker_view() {
            self.leave_worker();
            return;
        }
        if self.workers.is_empty() {
            self.push(Role::Notice, String::new(), "no agents yet: F4 opens a view once one starts".into());
            return;
        }
        let picked = self.selected.filter(|id| self.workers.iter().any(|worker| worker.id == *id));
        let Some(id) = picked.or_else(|| self.workers.first().map(|worker| worker.id)) else { return };
        self.open_worker(id);
    }

    /// ↑ and ↓ in the view: the previous or next worker, so every worker is reachable without a mouse.
    fn step_worker(&mut self, forward: bool) {
        let Some(id) = self.selected else { return };
        let Some(index) = self.workers.iter().position(|worker| worker.id == id) else { return };
        let last = self.workers.len().saturating_sub(1);
        let next = if forward { index.saturating_add(1).min(last) } else { index.saturating_sub(1) };
        if let Some(id) = self.workers.get(next).map(|worker| worker.id) {
            self.open_worker(id);
        }
    }

    /// Esc: back to the fleet, keeping the worker picked so F4 reopens the same one.
    pub(super) fn leave_worker(&mut self) {
        self.open = false;
        self.dirty = true;
    }

    /// One worker's view: a breadcrumb, its state, then its own transcript, which opens with the
    /// instruction the worker was started with.
    fn render_worker_view(&mut self, frame: &mut Frame, area: Rect) {
        let Some(worker) = self.focused().cloned() else { return };
        let bar =
            Block::new().borders(Borders::LEFT).border_set(SIDE_RULE).border_style(Style::new().fg(theme::accent()));
        let inner = bar.inner(area);
        frame.render_widget(bar, area);
        // The stream indents itself by two columns; the header uses the same inset, one row down.
        let top = Rect { y: inner.y.saturating_add(1), height: inner.height.saturating_sub(1), ..inner };
        let width = usize::from(top.width.saturating_sub(4));
        let (glyph, colour, state) = worker_state(&worker);
        let mut crumb = vec![
            Span::styled("agents ", Style::new().fg(theme::muted())),
            Span::styled("› ", Style::new().fg(theme::faint())),
            Span::styled(worker.label.clone(), Style::new().fg(theme::info())),
        ];
        if !worker.profile.is_empty() {
            crumb.push(Span::styled(format!(" · {}", worker.profile), Style::new().fg(theme::faint())));
        }
        let crumb = Line::from(crumb);
        let mut line = vec![
            Span::styled(format!("{glyph} "), Style::new().fg(colour)),
            Span::styled(state, Style::new().fg(theme::muted())),
            Span::styled(format!(" · {}", worker.dir.display()), Style::new().fg(theme::faint())),
        ];
        let hint = if self.stop_target() == Some(worker.id) { "esc stop · F4 back" } else { "esc back to agents" };
        let used = Line::from(line.clone()).width();
        line.push(Span::raw(" ".repeat(width.saturating_sub(used + text_width(hint)).max(1))));
        line.push(Span::styled(hint, Style::new().fg(theme::faint())));
        let head = vec![crumb, Line::from(line), Line::default()];
        // The header stays put; the worker's own stream scrolls under it, with its own position.
        let head_height = u16::try_from(head.len()).unwrap_or(u16::MAX).min(top.height);
        let header =
            Rect { x: top.x.saturating_add(2), width: top.width.saturating_sub(4), height: head_height, ..top };
        // The way out is on the state line, which is the second one.
        self.back_area = Rect { y: header.y.saturating_add(1), height: 1, ..header };
        frame.render_widget(Paragraph::new(head), header);
        let rest = Rect { y: top.y.saturating_add(head_height), height: top.height.saturating_sub(head_height), ..top };
        let (spinning, expand) = (self.spinning, self.tool_output);
        self.worker_stream_mut(worker.id).draw(frame, rest, spinning, expand);
    }

    /// The key, while a worker view is open: the view owns input, so nothing typed reaches the prompt.
    pub(super) fn worker_key(&mut self, key: KeyEvent) {
        self.dirty = true;
        match key.code {
            // Esc with nothing to stop leaves the view; it never quits the app.
            KeyCode::Esc => self.open = false,
            KeyCode::F(4) => self.toggle_worker_view(),
            // The view scrolls itself: each stream keeps its own position and tail state.
            KeyCode::PageUp => self.scroll_up(),
            KeyCode::PageDown => self.scroll_down(),
            KeyCode::Home => self.scroll_top(),
            KeyCode::End => self.scroll_bottom(),
            KeyCode::Down | KeyCode::Tab => self.step_worker(true),
            KeyCode::Up | KeyCode::BackTab => self.step_worker(false),
            _ => {}
        }
    }

    /// The prompt as submitted, trimmed, with any pasted image's path; the composer starts empty.
    pub(super) fn take_prompt(&mut self) -> String {
        let mut text = self.composer.lines().join("\n").trim().to_string();
        let path = match self.attachment.take() {
            Some(Attachment::Ready { path, .. }) => Some(path),
            // A read still in flight, or a chip that found nothing, has no file to name.
            _ => None,
        };
        // Whatever the chip held is spent: a read that lands after this message belongs to none.
        self.paste = self.paste.wrapping_add(1);
        if let Some(path) = path {
            // The chat-completions request carries text only: the picture rides along as the path of a
            // file the model can open with its own tools.
            let note = format!("[attached image: {}]", path.display());
            text = if text.is_empty() { note } else { format!("{text}\n\n{note}") };
        }
        let typed = self.composer.lines().join("\n");
        self.composer = composer();
        self.recall = None;
        self.completion = None;
        // History holds what was typed: a recalled temp path would point at a file since dropped.
        if !typed.is_empty() {
            self.history.push(typed);
        }
        self.dirty = true;
        text
    }

    pub(super) fn echo(&mut self, prompt: &str) {
        self.push(Role::You, String::new(), prompt.to_string());
        self.dirty = true;
    }

    pub(super) fn insert(&mut self, text: &str) {
        self.composer.insert_str(text);
        self.dirty = true;
        self.refresh_completion();
    }

    /// A bracketed paste: the composer takes it, unless a worker's view owns the keys.
    pub(super) fn paste(&mut self, text: &str) {
        if self.in_worker_view() {
            self.notice("paste goes to the prompt: esc leaves this view first");
            return;
        }
        self.insert(text);
    }

    /// Ctrl+V, the instant the key lands: the chip goes up before the clipboard is read.
    ///
    /// The read spawns `osascript` and copies the whole image, which would freeze the frame for as
    /// long as it runs; it belongs on a thread of its own. Returns the id the read must carry back,
    /// or nothing while one is already in flight, so a held key cannot start a second.
    pub(super) fn begin_paste(&mut self) -> Option<u64> {
        if matches!(self.attachment, Some(Attachment::Reading)) {
            return None;
        }
        self.clear_attachment();
        self.paste = self.paste.wrapping_add(1);
        self.attachment = Some(Attachment::Reading);
        self.dirty = true;
        Some(self.paste)
    }

    /// Takes what a paste found: an image to attach, text for the composer, or why there is neither.
    ///
    /// A read whose id went stale was dropped or replaced while it ran: it shows nothing, and a file
    /// it wrote is removed, since no message will ever name it.
    pub(super) fn attach(&mut self, id: u64, paste: clipboard::Paste) {
        if id != self.paste {
            if let clipboard::Paste::Image(path) = paste {
                clipboard::discard(&path);
            }
            return;
        }
        self.clear_attachment();
        match paste {
            clipboard::Paste::Text(text) => self.insert(&text),
            clipboard::Paste::Image(path) => {
                let info = clipboard::info(&path);
                self.attachment = Some(Attachment::Ready { path, info });
            }
            clipboard::Paste::Empty => {
                self.attachment = Some(Attachment::Failed(NO_CLIPBOARD.to_string()));
                self.notice(NO_CLIPBOARD);
            }
            clipboard::Paste::Failed(why) => {
                self.attachment = Some(Attachment::Failed(why.clone()));
                self.push(Role::Error, String::new(), format!("cannot read the clipboard: {why}"));
            }
        }
        self.dirty = true;
    }

    /// Whether the clipboard is being read, which keeps the chip's spinner turning.
    fn reading(&self) -> bool {
        matches!(self.attachment, Some(Attachment::Reading))
    }

    /// Drops the attachment and its temp file.
    pub(super) fn detach(&mut self) {
        if self.attachment.is_none() {
            return;
        }
        self.clear_attachment();
        // A read still in flight lands as nobody's once its id is stale.
        self.paste = self.paste.wrapping_add(1);
        self.dirty = true;
    }

    /// Drops what the chip holds, removing a pasted file, and keeps the read's id as it is.
    fn clear_attachment(&mut self) {
        if let Some(path) = self.attachment.take().as_ref().and_then(Attachment::path) {
            clipboard::discard(path);
        }
    }

    /// Backspace drops the pending attachment once the composer is empty, before it eats nothing.
    pub(super) fn backspace(&mut self, key: KeyEvent) {
        if self.attachment.is_some() && self.composer.lines().iter().all(String::is_empty) {
            self.detach();
        } else {
            self.input(key);
        }
    }

    fn notice(&mut self, text: &str) {
        self.push(Role::Notice, String::new(), text.to_string());
        self.dirty = true;
    }

    pub(super) fn input(&mut self, key: KeyEvent) {
        self.composer.input(key);
        self.dirty = true;
        self.refresh_completion();
    }

    pub(super) fn toggle_workers(&mut self) {
        self.worker_panel = !self.worker_panel;
        self.dirty = true;
    }

    /// The width the panel opens at, as the last run left it; the launch width when it never moved.
    pub(super) fn set_workers_width(&mut self, width: u16) {
        self.workers_width = width.max(WORKERS_MIN);
        self.dirty = true;
    }

    /// The width the panel is set to, which a resize of the terminal never narrows: it is what the
    /// next run comes back to.
    pub(super) fn workers_width(&self) -> u16 {
        self.workers_width
    }

    /// Whether the width moved since the last call, so the frontend writes it down once per drag.
    pub(super) fn take_width_dirty(&mut self) -> bool {
        std::mem::take(&mut self.width_dirty)
    }

    /// The width the panel takes this frame, or `None` when it is hidden or the terminal has no room
    /// left for a readable transcript beside it.
    fn panel_width(&self, total: u16) -> Option<u16> {
        let room = total.checked_sub(TRANSCRIPT_MIN)?;
        (self.worker_panel && room >= WORKERS_MIN).then(|| self.workers_width.clamp(WORKERS_MIN, room))
    }

    /// A drag of the panel's rule: the width follows the pointer, bounded so neither side vanishes.
    ///
    /// The transcript rewraps to the columns it has left, so the whole cache is dropped rather than
    /// scrolled through at the old width.
    pub(super) fn drag(&mut self, column: u16) {
        if !self.resizing || self.split.is_none() {
            return;
        }
        let right = self.body_area.x.saturating_add(self.body_area.width);
        let room = self.body_area.width.saturating_sub(TRANSCRIPT_MIN).max(WORKERS_MIN);
        let width = right.saturating_sub(column).clamp(WORKERS_MIN, room);
        if width == self.workers_width {
            return;
        }
        self.workers_width = width;
        self.width_dirty = true;
        self.resize();
    }

    /// The pointer let go: the panel keeps the width it was dragged to.
    pub(super) fn release(&mut self) {
        self.resizing = false;
    }

    /// F5: captures the mouse for clicks and the wheel, or hands it back to the terminal.
    ///
    /// While it is captured, dragging in the transcript selects nothing: the terminal never sees
    /// the drag. Returns the new state, which the frontend mirrors on the terminal.
    pub(super) fn toggle_mouse(&mut self) -> bool {
        self.mouse = !self.mouse;
        let line = if self.mouse {
            "mouse captured: F5 hands it back to the terminal"
        } else {
            "mouse released: select and copy with the terminal, F5 captures it again"
        };
        // The reader is looking at one stream: a worker's view has its own, and the lead's is behind it.
        self.active().push(Entry::new(Role::Notice, String::new(), line.to_string()));
        self.dirty = true;
        self.mouse
    }

    /// Hides or shows what tool calls returned, and collapses the body of a long card; every entry
    /// changes height, so the cache is dropped.
    pub(super) fn toggle_tool_output(&mut self) {
        self.tool_output = !self.tool_output;
        self.lead.resize();
        for worker in &mut self.worker_streams {
            worker.stream.resize();
        }
        self.dirty = true;
    }

    pub(super) fn drain(&mut self) {
        self.draining = true;
        self.dirty = true;
    }

    /// The core thread vanished without saying `Done`: show it and quit rather than wait forever.
    pub(super) fn lost_core(&mut self) {
        self.push(Role::Error, String::new(), "overseer stopped unexpectedly".into());
        self.dirty = true;
        self.quit = true;
    }

    pub(super) fn scroll_up(&mut self) {
        self.active().scroll_up();
        self.dirty = true;
    }

    pub(super) fn scroll_down(&mut self) {
        self.active().scroll_down();
        self.dirty = true;
    }

    pub(super) fn scroll_top(&mut self) {
        self.active().scroll_top();
        self.dirty = true;
    }

    pub(super) fn scroll_bottom(&mut self) {
        self.active().scroll_bottom();
        self.dirty = true;
    }

    pub(super) fn wheel(&mut self, up: bool) {
        self.active().wheel(up);
        self.dirty = true;
    }

    pub(super) fn recall_previous(&mut self) {
        let Some(last) = self.history.len().checked_sub(1) else {
            return;
        };
        let index = self.recall.map_or(last, |i| i.saturating_sub(1));
        if let Some(text) = self.history.get(index).cloned() {
            self.refill(&text);
            self.recall = Some(index);
        }
    }

    pub(super) fn recall_next(&mut self) {
        let Some(index) = self.recall else {
            return;
        };
        if index + 1 < self.history.len() {
            if let Some(text) = self.history.get(index + 1).cloned() {
                self.refill(&text);
                self.recall = Some(index + 1);
            }
        } else {
            self.composer = composer();
            self.completion = None;
            self.recall = None;
            self.dirty = true;
        }
    }

    /// Replaces the composer's text with a recalled prompt, caret at its end.
    fn refill(&mut self, text: &str) {
        self.fill(text);
        self.refresh_completion();
    }

    /// Puts `text` in the composer, one line per newline, caret at the end.
    fn fill(&mut self, text: &str) {
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let cursor = (lines.len().saturating_sub(1), lines.last().map_or(0, |line| line.chars().count()));
        self.composer.set_lines(lines, cursor);
        self.dirty = true;
    }

    /// Whether the completion popup is open: it owns Up, Down, Tab, Enter and Esc while it is.
    pub(super) fn completing(&self) -> bool {
        self.completion.is_some()
    }

    /// Up and Down move the popup's highlight; with it closed the key reaches the composer as before.
    pub(super) fn step_completion(&mut self, forward: bool) {
        if let Some(menu) = &mut self.completion {
            menu.step(forward);
            self.dirty = true;
        }
    }

    /// Esc closes the popup and leaves what was typed where it is: the keys the user chose stay.
    pub(super) fn close_completion(&mut self) {
        if self.completion.take().is_some() {
            self.dirty = true;
        }
    }

    /// Tab and Enter while the popup is open: the highlighted row lands in the composer, caret at
    /// its end, and the popup closes. Returns whether there was a row to accept.
    pub(super) fn accept_completion(&mut self) -> bool {
        let Some(insertion) = self.completion.as_ref().and_then(Menu::picked).map(Candidate::insertion) else {
            return false;
        };
        self.fill(&insertion);
        // The row is spent: the next Enter submits what it left behind, and the popup does not
        // reopen on the row that just came from it.
        self.completion = None;
        self.dirty = true;
        true
    }

    /// Rebuilds the popup from the composer's text and caret, after a key changed either.
    fn refresh_completion(&mut self) {
        // The keystroke changed the list, so the highlight starts again on its first row.
        self.completion = self.menu();
    }

    /// The popup the composer asks for, if any: the first token while it still starts with `/`, or
    /// the ids `/resume` takes once its space has been typed.
    fn menu(&self) -> Option<Menu> {
        let [line] = self.composer.lines() else { return None };
        // What lies behind the caret: a space is what ends the command being typed.
        let (_, column) = self.composer.cursor();
        let typed: String = line.chars().take(column).collect();
        if let Some(rest) = typed.strip_prefix(RESUME).and_then(|rest| rest.strip_prefix(' '))
            && !rest.contains(' ')
        {
            return Menu::agents(&self.workers, rest);
        }
        let token = typed.rsplit(' ').next().unwrap_or_default();
        if token.starts_with('/') && typed == token {
            return Menu::commands(token);
        }
        None
    }
}

fn worker_state(worker: &AgentView) -> (&'static str, Color, String) {
    // The rights ride on the state word: a supervisor never writes and a read-only worker never
    // does either, so the panel spends one line on both.
    let rights = if worker.supervisor {
        " · supervisor"
    } else if worker.read_only {
        " · read-only"
    } else {
        ""
    };
    match worker.state {
        State::Stopped => ("■", theme::faint(), "stopped".into()),
        State::Archived => ("·", theme::faint(), "archived".into()),
        State::Closed => ("◼", theme::muted(), format!("closed{rights}")),
        State::Paused => ("❚❚", theme::info(), format!("paused{rights}")),
        State::Working => {
            let secs = worker.busy_for.unwrap_or_default().as_secs();
            ("●", theme::warning(), format!("working {secs}s{rights}"))
        }
        State::Failed => ("✕", theme::error(), "failed".into()),
        State::Idle | State::Done => ("○", theme::faint(), format!("idle{rights}")),
    }
}

/// The last component of the worker's directory, so a narrow panel stays readable.
fn short_dir(dir: &Path) -> String {
    dir.file_name().map_or_else(|| dir.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// An agent's directory with the home prefix written `~`, so a completion row stays short and still
/// says where the agent sits. A home the run cannot name leaves the path as it is.
fn shown_dir(dir: &Path) -> String {
    let Ok(home) = crate::config::home() else { return dir.display().to_string() };
    match dir.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

/// The longest prefix of `hints` that fits `room` columns, joined the way the status line shows them.
fn fit_hints(hints: &[&str], room: usize) -> String {
    let mut fitted = String::new();
    for hint in hints {
        let next = if fitted.is_empty() { (*hint).to_string() } else { format!("{fitted} · {hint}") };
        if text_width(&next) > room {
            break;
        }
        fitted = next;
    }
    fitted
}

#[cfg(test)]
mod paste_tests {
    use super::*;
    use clipboard::Paste;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A PNG header the chip can read a size from.
    fn saved(dir: &Path) -> PathBuf {
        let path = dir.join("paste-1-1.png");
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&2560_u32.to_be_bytes());
        png.extend_from_slice(&1600_u32.to_be_bytes());
        fs::write(&path, &png).expect("a png");
        path
    }

    /// A file where a real paste writes it, so the chip can drop it: `discard` only removes its own.
    fn pasted() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = crate::clipboard::pasted(&format!(
            "chip-test-{}-{}.png",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"\x89PNG\r\n\x1a\n").expect("a png");
        path
    }

    /// What the loop does with a read that landed: the paste's own id, then its answer.
    fn land(app: &mut App, paste: Paste) {
        let id = app.begin_paste().expect("nothing else is being read");
        app.attach(id, paste);
    }

    fn chip(app: &App) -> String {
        match &app.attachment {
            Some(Attachment::Ready { info, .. }) => info.chip(),
            Some(Attachment::Reading) => ATTACHMENT_READING.to_string(),
            Some(Attachment::Failed(why)) => format!("failed: {why}"),
            None => String::new(),
        }
    }

    #[test]
    fn a_pasted_screenshot_becomes_a_pending_attachment() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = App::new("m");
        let path = saved(dir.path());
        land(&mut app, Paste::Image(path.clone()));
        match &app.attachment {
            Some(Attachment::Ready { path: kept, info }) => {
                assert_eq!(kept, &path);
                assert_eq!(info.name, "paste-1-1.png");
                assert_eq!(info.pixels, Some((2560, 1600)));
            }
            other => panic!("expected a ready attachment: {:?}", other.is_some()),
        }
    }

    #[test]
    fn the_chip_is_up_before_the_clipboard_is_read() {
        let mut app = App::new("m");
        let id = app.begin_paste().expect("the first paste reads");
        assert!(matches!(app.attachment, Some(Attachment::Reading)), "the chip shows the read in flight");
        assert!(app.take_dirty(), "the frame is due in the tick the key landed");
        assert!(app.begin_paste().is_none(), "a held key never starts a second read");
        app.attach(id, Paste::Empty);
        assert!(matches!(app.attachment, Some(Attachment::Failed(_))), "the read replaces the pending chip");
    }

    #[test]
    fn a_read_whose_chip_was_dismissed_lands_as_nobodys() {
        let mut app = App::new("m");
        let id = app.begin_paste().expect("a read");
        app.detach();
        let path = pasted();
        app.attach(id, Paste::Image(path.clone()));
        assert!(app.attachment.is_none(), "the chip stays dismissed");
        assert!(!path.exists(), "and the file it wrote is dropped, since no message will name it");
    }

    #[test]
    fn a_late_image_never_rides_a_message_sent_before_it_landed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = App::new("m");
        let id = app.begin_paste().expect("a read");
        app.insert("what is this?");
        assert_eq!(app.take_prompt(), "what is this?", "a read in flight has no file to name");
        app.attach(id, Paste::Image(saved(dir.path())));
        assert!(app.attachment.is_none(), "the message that left never carried it");
    }

    #[test]
    fn an_empty_clipboard_says_so_instead_of_doing_nothing() {
        let mut app = App::new("m");
        land(&mut app, Paste::Empty);
        assert!(matches!(&app.attachment, Some(Attachment::Failed(why)) if why == NO_CLIPBOARD));
        let last = app.lead.entries.last().expect("a notice");
        assert_eq!(last.role, Role::Notice);
        assert_eq!(last.body, NO_CLIPBOARD);
    }

    #[test]
    fn a_clipboard_that_cannot_be_read_says_why() {
        let mut app = App::new("m");
        land(&mut app, Paste::Failed("osascript: not found".into()));
        let last = app.lead.entries.last().expect("a notice");
        assert_eq!(last.body, "cannot read the clipboard: osascript: not found");
        assert!(matches!(&app.attachment, Some(Attachment::Failed(why)) if why.contains("not found")));
    }

    #[test]
    fn clipboard_text_goes_into_the_composer_and_leaves_no_chip() {
        let mut app = App::new("m");
        land(&mut app, Paste::Text("a sentence".into()));
        assert_eq!(app.take_prompt(), "a sentence");
        assert!(app.attachment.is_none(), "text is not an attachment");
    }

    #[test]
    fn ctrl_x_drops_a_ready_attachment_and_its_temp_file() {
        let mut app = App::new("m");
        let path = pasted();
        land(&mut app, Paste::Image(path.clone()));
        app.detach();
        assert!(app.attachment.is_none());
        assert!(!path.exists(), "the temp file goes with the chip");
    }

    #[test]
    fn the_chip_names_the_file_its_size_and_its_pixels() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = App::new("m");
        land(&mut app, Paste::Image(saved(dir.path())));
        let text = chip(&app);
        assert!(text.starts_with("paste-1-1.png · 2560×1600 · "), "{text}");
        assert!(text.ends_with(" B"), "a 24-byte header is under a kilobyte: {text}");
    }

    #[test]
    fn an_attachment_rides_one_message_as_its_absolute_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut app = App::new("m");
        let path = saved(dir.path());
        land(&mut app, Paste::Image(path.clone()));
        app.insert("what is this?");
        let prompt = app.take_prompt();
        assert!(prompt.starts_with("what is this?"), "{prompt}");
        assert!(prompt.contains("[attached image: "), "{prompt}");
        assert!(prompt.contains(".png]"), "{prompt}");
        assert!(app.attachment.is_none(), "the attachment belongs to one message");
        assert!(path.exists(), "the file is the message's: it outlives the chip");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use std::fmt::Write as _;
    use std::path::PathBuf;

    /// One frame drawn into a real backend, for the tests that read cells rather than text.
    fn drawn(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let buffer = drawn(app, width, height);
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buffer.cell((x, y)).map_or(" ", ratatui::buffer::Cell::symbol));
            }
            out.push('\n');
        }
        out
    }

    fn row_text(lines: &[Line<'_>]) -> String {
        lines.iter().flat_map(|line| line.spans.iter().map(|span| span.content.as_ref())).collect()
    }

    #[test]
    fn wrap_keeps_words_and_cuts_oversized_ones() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("  indented", 20), ["  indented"]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn an_expanded_instruction_to_a_worker_is_shown_whole_across_rows() {
        let mut app = App::new("m");
        let text = format!("first line\n{}tail", "word ".repeat(40));
        tool_call(&mut app, 0, "message_agent", &format!("#2 · {text}"));
        let lines = app.lead.entries[0].lines(40, 0, true);
        assert!(lines.len() > 2, "a long instruction wraps: {}", lines.len());
        let joined = row_text(&lines);
        assert!(joined.contains("first line"), "{joined}");
        assert!(joined.contains("tail"), "the end of the message is not cut: {joined}");
        assert!(!joined.contains("…"), "nothing is elided: {joined}");
        app.toggle_tool_output();
        let rendered = screen(&mut app, 100, 24);
        assert!(rendered.contains("first line"), "{rendered}");
        assert!(rendered.contains("tail"), "the rendered screen shows the end: {rendered}");
    }

    #[test]
    fn a_tool_card_is_one_row_until_expanded() {
        let mut app = App::new("m");
        let text = format!("first line\n{}tail", "word ".repeat(40));
        tool_call(&mut app, 0, "spawn_agent", &format!("/tmp · {text}"));
        let collapsed = app.lead.entries[0].lines(40, 0, false);
        assert_eq!(collapsed.len(), 1, "a compact card is its header: {collapsed:?}");
        let joined = row_text(&collapsed);
        assert!(joined.contains("first line"), "{joined}");
        assert!(!joined.contains("tail"), "the end of the task waits for ctrl+0: {joined}");
        let whole = app.lead.entries[0].lines(40, 0, true);
        assert!(whole.len() > 1, "ctrl+0 shows the whole body: {whole:?}");
        assert!(row_text(&whole).contains("tail"), "the expanded card reaches the end");
    }

    #[test]
    fn a_shell_command_is_shown_whole_at_rest() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, "bash", "cargo test\ncargo build");
        let collapsed = row_text(&app.lead.entries[0].lines(80, 0, false));
        assert!(collapsed.contains("cargo test"), "{collapsed}");
        assert!(collapsed.contains("cargo build"), "the whole command, not its first line: {collapsed}");
        assert!(!collapsed.contains('…'), "nothing is elided: {collapsed}");
    }

    #[test]
    fn prose_renders_markdown_and_drops_its_markers() {
        let lines = markdown("# Title\n\n**bold** and `code`\n\n- one\n- two", "  ", Style::new(), 60);
        let text = row_text(&lines);
        assert!(text.contains("Title"), "{text}");
        assert!(!text.contains('#'), "the heading marker is gone: {text}");
        assert!(text.contains("bold"), "{text}");
        assert!(!text.contains("**"), "the emphasis marker is gone: {text}");
        assert!(text.contains("code"), "{text}");
        assert!(text.contains("one") && text.contains("two"), "{text}");
        let heading = lines.iter().flat_map(|line| &line.spans).find(|span| span.content == "Title");
        let heading = heading.expect("the heading survives");
        assert_eq!(heading.style.fg, Some(theme::accent()), "headings take the accent");
        assert!(heading.style.add_modifier.contains(Modifier::BOLD), "headings are bold");

        let mut app = App::new("m");
        app.apply(UiEvent::Answer("# Title\n\n**bold** and `code`".into()));
        let rendered = screen(&mut app, 80, 20);
        assert!(rendered.contains("Title") && rendered.contains("bold"), "{rendered}");
        assert!(!rendered.contains('#') && !rendered.contains("**"), "the screen shows prose: {rendered}");
    }

    #[test]
    fn tables_and_fences_render_without_the_source_syntax() {
        let table = markdown("| a | b |\n|---|---|\n| 1 | 2 |", "  ", Style::new(), 40);
        let table = row_text(&table);
        for cell in ["a", "b", "1", "2", "│"] {
            assert!(table.contains(cell), "the table keeps {cell}: {table}");
        }
        assert!(!table.contains("|---"), "the delimiter row is syntax, not output: {table}");
        let code = markdown("```rust\nfn main() {}\n```", "  ", Style::new(), 40);
        let code = row_text(&code);
        assert!(code.contains("fn main() {}"), "{code}");
        assert!(!code.contains("```") && !code.contains("rust"), "the fence and its language stay hidden: {code}");
    }

    #[test]
    fn markdown_wrapping_keeps_each_run_style() {
        let style = Style::new().fg(theme::warning()).add_modifier(Modifier::BOLD);
        let line = Line::from(Span::styled("alpha beta gamma delta".to_string(), style));
        let rows = wrap_styled(&line, 10);
        assert!(rows.len() > 1, "the line wraps: {rows:?}");
        for span in rows.iter().flat_map(|row| &row.spans) {
            assert_eq!(span.style, style, "no row loses the run's colour or weight");
        }
    }

    #[test]
    fn tool_output_is_never_parsed_as_markdown() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, "read_file", "**literal**");
        let text = row_text(&app.lead.entries[0].lines(80, 0, true));
        assert!(text.contains("**literal**"), "a file's characters reach the screen as written: {text}");
    }

    #[test]
    fn an_internal_uri_is_drawn_in_the_colour_of_its_scheme() {
        let spans = highlight("read `conflict://1` then skill://x", Style::new().fg(theme::text()));
        let joined: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, "read `conflict://1` then skill://x", "splitting changes no character");
        let colour = |token: &str| spans.iter().find(|span| span.content == token).and_then(|span| span.style.fg);
        assert_eq!(colour("conflict://1"), Some(theme::warning()), "a conflict is a warning");
        assert_eq!(colour("skill://x"), Some(theme::accent()), "a skill is the overseer's accent");
    }

    #[test]
    fn apply_pairs_tool_calls_with_their_results() {
        let mut app = App::new("m");
        tool_call(&mut app, 3, "spawn_agent", "/tmp · do it");
        let started = row_text(&app.lead.entries[0].lines(80, 0, true));
        assert!(started.contains("⠋ spawn_agent /tmp · do it"), "{started}");
        tool_result(&mut app, 3, "spawn_agent", "agent 2 started", 0.4);
        let finished = row_text(&app.lead.entries[0].lines(80, 0, true));
        assert!(finished.contains("✓ spawn_agent /tmp · do it"), "{finished}");
        assert!(finished.contains("0.4s"), "{finished}");
        assert!(finished.contains("│ agent 2 started"), "{finished}");
    }

    #[test]
    fn an_agent_tool_call_shows_its_command_then_the_head_of_its_output() {
        let mut app = App::new("m");
        let started = Progress::Started { call: 7, tool: tools::BASH.into(), summary: "cargo test".into() };
        app.apply(UiEvent::Progress { id: 2, progress: started });
        assert!(app.lead.entries.is_empty(), "an agent's tool call never reaches the lead's stream");
        let head = row_text(&worker_entries(&app, 2)[0].lines(80, 0, true));
        assert!(head.contains("$ cargo test"), "{head}");
        let output = "exit code: 0\nstdout:\none\ntwo\nthree\nfour\nfive\nsix\nstderr:\n";
        let preview = tools::preview(tools::BASH, output);
        let finished = Progress::Finished { call: 7, secs: 2.4, preview };
        app.apply(UiEvent::Progress { id: 2, progress: finished });
        let text = row_text(&worker_entries(&app, 2)[0].lines(80, 0, true));
        assert!(text.contains("✓ $ cargo test"), "{text}");
        assert!(text.contains("2.4s"), "{text}");
        assert!(text.contains("│ one"), "{text}");
        assert!(!text.contains("five"), "only the head is shown: {text}");
        assert!(text.contains("… 2 more lines"), "{text}");
        assert!(!text.contains("stdout:"), "the result's scaffolding is not output: {text}");
        assert_eq!(worker_entries(&app, 2)[0].lines(80, 0, false).len(), 1, "a compact card is one row per call");
    }

    #[test]
    fn an_edit_card_aligns_its_gutter_and_colours_each_side() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, tools::EDIT, "src/main.rs");
        tool_result(&mut app, 0, tools::EDIT, "edit: +1/-1\n-2|old\n+2|new\n 12|context", 0.4);
        let lines = app.lead.entries[0].lines(80, 0, true);
        let joined = row_text(&lines);
        assert!(joined.contains("✓ edit src/main.rs ( +1/-1 )"), "{joined}");
        assert!(joined.contains("- 2│old"), "the removal keeps its line: {joined}");
        assert!(joined.contains("+ 2│new"), "the addition keeps its line: {joined}");
        assert!(joined.contains(" 12│context"), "a context row is numbered but unmarked: {joined}");
        assert_eq!(lines[1].spans[2].style.fg, Some(theme::error()), "a removal is red");
        assert_eq!(lines[2].spans[2].style.fg, Some(theme::success()), "an addition is green");
        assert_eq!(lines[3].spans[2].style.fg, Some(theme::muted()), "context is muted");
        let rendered = screen(&mut app, 100, 24);
        assert!(rendered.contains("✓ edit src/main.rs ( +1/-1 )"), "{rendered}");
        assert!(rendered.contains("- 2│old"), "{rendered}");
    }

    #[test]
    fn a_collapsed_edit_card_stops_at_ten_rows_and_counts_the_rest() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, tools::EDIT, "t.rs");
        let mut output = String::from("edit: +15/-0\n");
        for number in 1..=15 {
            let _ = writeln!(output, "+{number}|row {number}");
        }
        tool_result(&mut app, 0, tools::EDIT, &output, 0.1);
        let collapsed = app.lead.entries[0].lines(80, 0, false);
        assert_eq!(collapsed.len(), 12, "a header, ten rows and the count: {collapsed:?}");
        assert!(row_text(&collapsed).contains("… 5 more lines (ctrl+o to expand)"), "{}", row_text(&collapsed));
        let expanded = app.lead.entries[0].lines(80, 0, true);
        assert_eq!(expanded.len(), 16, "a header and every row: {}", expanded.len());
    }

    #[test]
    fn an_edit_card_renders_a_trailer_and_a_numberless_row() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, tools::EDIT, "t.rs");
        tool_result(&mut app, 0, tools::EDIT, "edit: +0/-2\n-1|a\n-2|b\nremoved t.rs", 0.1);
        let joined = row_text(&app.lead.entries[0].lines(80, 0, true));
        assert!(joined.contains("│removed t.rs"), "a trailer keeps the gutter: {joined}");

        let mut app = App::new("m");
        tool_call(&mut app, 0, tools::EDIT, "t.rs");
        tool_result(&mut app, 0, tools::EDIT, "edit: +1/-0\n+|bad", 0.1);
        let joined = row_text(&app.lead.entries[0].lines(80, 0, true));
        assert!(joined.contains("+│bad"), "a numberless addition does not panic: {joined}");
    }

    #[test]
    fn a_failed_command_shows_its_exit_code_and_error() {
        let mut app = App::new("m");
        let started = Progress::Started { call: 0, tool: tools::BASH.into(), summary: "cargo build".into() };
        app.apply(UiEvent::Progress { id: 2, progress: started });
        let output = "exit code: 101\nstdout:\nstderr:\nerror[E0308]: mismatched types\n";
        let preview = tools::preview(tools::BASH, output);
        let finished = Progress::Finished { call: 0, secs: 0.5, preview };
        app.apply(UiEvent::Progress { id: 2, progress: finished });
        let text = row_text(&worker_entries(&app, 2)[0].lines(80, 0, true));
        assert!(text.contains("✕ $ cargo build"), "{text}");
        assert!(text.contains("exit 101"), "{text}");
        assert!(text.contains("error[E0308]"), "{text}");
    }

    #[test]
    fn a_dead_agent_closes_the_call_it_left_running() {
        let mut app = App::new("m");
        let started = Progress::Started { call: 0, tool: tools::BASH.into(), summary: "sleep 900".into() };
        app.apply(UiEvent::Progress { id: 2, progress: started });
        let mut dead = agent_view(2, "wait");
        dead.state = State::Failed;
        dead.busy_for = Some(Duration::from_secs(9));
        app.apply(UiEvent::Agents(vec![dead]));
        assert!(app.calls.is_empty(), "a call that can never return is unpaired");
        let text = row_text(&worker_entries(&app, 2)[0].lines(80, 0, true));
        assert!(text.contains("no result"), "{text}");
        assert!(!text.contains('⠋'), "a stopped agent never keeps the spinner turning: {text}");
    }

    #[test]
    fn streamed_text_is_one_entry_that_the_answer_completes() {
        let mut app = App::new("m");
        delta(&mut app, "Worker ");
        delta(&mut app, "1 is done.");
        assert_eq!(app.lead.entries.len(), 1);
        app.apply(UiEvent::Answer("Worker 1 is done.".into()));
        assert_eq!(app.lead.entries.len(), 1);
        assert_eq!(app.lead.entries[0].body, "Worker 1 is done.");
    }

    #[test]
    fn model_call_boundary_becomes_a_blank_line() {
        let mut app = App::new("m");
        delta(&mut app, "first call");
        app.apply(UiEvent::Boundary { id: Swarm::ROOT });
        delta(&mut app, "second call");
        assert_eq!(app.lead.entries[0].body, "first call\n\nsecond call");
        assert_eq!(app.lead.entries[0].lines(40, 0, true).len(), 3);
    }

    #[test]
    fn text_streamed_after_a_tool_call_lands_below_its_card() {
        let mut app = App::new("m");
        delta(&mut app, "checking the config");
        tool_call(&mut app, 0, "bash", "cargo test");
        tool_result(&mut app, 0, "bash", "ok", 1.0);
        app.apply(UiEvent::Boundary { id: Swarm::ROOT });
        delta(&mut app, "all green");
        app.apply(UiEvent::Answer("all green".into()));
        let bodies: Vec<&str> = app.lead.entries.iter().map(|entry| entry.body.as_str()).collect();
        assert_eq!(bodies, ["checking the config", "cargo test", "all green"]);
    }

    #[test]
    fn a_notice_ends_the_streamed_paragraph() {
        let mut app = App::new("m");
        delta(&mut app, "before");
        app.apply(UiEvent::Notice("agent 2 started".into()));
        delta(&mut app, "after");
        let bodies: Vec<&str> = app.lead.entries.iter().map(|entry| entry.body.as_str()).collect();
        assert_eq!(bodies, ["before", "agent 2 started", "after"]);
    }

    #[test]
    fn a_boundary_before_any_text_opens_no_entry() {
        let mut app = App::new("m");
        app.apply(UiEvent::Boundary { id: Swarm::ROOT });
        assert!(app.lead.entries.is_empty());
    }

    #[test]
    fn a_literal_newline_delta_stays_text() {
        let mut app = App::new("m");
        delta(&mut app, "a");
        delta(&mut app, "\n");
        delta(&mut app, "b");
        assert_eq!(app.lead.entries[0].body, "a\nb");
    }

    #[test]
    fn a_new_turn_never_extends_the_previous_stream() {
        let mut app = App::new("m");
        delta(&mut app, "first turn");
        app.apply(UiEvent::Busy(false));
        delta(&mut app, "second turn");
        assert_eq!(app.lead.entries.len(), 2);
        assert_eq!(app.lead.entries[0].body, "first turn");
        assert_eq!(app.lead.entries[1].body, "second turn");
    }

    #[test]
    fn answer_without_stream_still_appears() {
        let mut app = App::new("m");
        app.apply(UiEvent::Answer("plain reply".into()));
        assert_eq!(app.lead.entries.len(), 1);
        assert_eq!(app.lead.entries[0].body, "plain reply");
    }

    #[test]
    fn renders_transcript_agent_panel_and_status() {
        let mut app = App::new("test-model");
        app.apply(UiEvent::Notice("agent logs in /tmp/logs".into()));
        delta(&mut app, "spawned ");
        delta(&mut app, "two agents");
        let mut child = agent_view(2, "run the tests");
        child.state = State::Done;
        child.dir = PathBuf::from("/tmp/overseer");
        app.apply(UiEvent::Agents(vec![child]));
        app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Finished, text: "tests pass".into() });
        let screen = screen(&mut app, 100, 24);
        assert!(screen.contains("spawned two agents"), "{screen}");
        assert!(screen.contains(&format!("agent 2 · {} finished: tests pass", names::name(2, None))), "{screen}");
        assert!(screen.contains("overseer"), "{screen}");
        assert!(screen.contains("idle"), "{screen}");
        assert!(screen.contains("┃"), "the composer keeps its bar: {screen}");
        assert!(screen.contains("Ask agent 1"), "{screen}");
        assert!(screen.contains("test-model"), "the status line names the model: {screen}");
        assert!(screen.contains(version::VERSION), "{screen}");
    }

    #[test]
    fn an_agent_line_carries_its_codename_beside_the_id() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agent { id: 3, kind: AgentEvent::Finished, text: "done".into() });
        let name = names::name(3, None);
        let rendered = screen(&mut app, 100, 24);
        assert!(rendered.contains(&format!("agent 3 · {name} finished")), "{rendered}");
        assert_eq!(worker_entries(&app, 3)[0].title, format!("agent 3 · {name}"), "the id stays first");
    }

    #[test]
    fn the_panel_and_a_view_name_an_agent_by_its_label() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "run the tests")]));
        let fleet = screen(&mut app, 100, 24);
        assert!(fleet.contains(&names::label(2, None)), "{fleet}");
        click_first_agent(&mut app);
        let view = screen(&mut app, 100, 24);
        assert!(view.contains(&format!("agents › {}", names::label(2, None))), "{view}");
    }

    #[test]
    fn the_rename_prompt_takes_a_name_and_escape_cancels_on_the_picked_agent() {
        let mut app = App::new("m");
        assert!(!app.renaming());
        app.start_rename();
        assert!(app.rename.is_none(), "nothing to rename before an agent exists");
        assert!(app.lead.entries.last().is_some_and(|entry| entry.body.contains("no agent to rename")));

        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "first job"), agent_view(3, "second job")]));
        // A click picks an agent; the prompt renames that one, not the newest.
        click_first_agent(&mut app);
        app.leave_worker();
        app.start_rename();
        assert_eq!(app.rename.as_ref().map(|rename| rename.id), Some(2));
        let rendered = screen(&mut app, 100, 24);
        assert!(rendered.contains(&format!("rename {}", names::label(2, None))), "{rendered}");
        assert!(rendered.contains("enter accept") && rendered.contains("esc cancel"), "{rendered}");
        for character in "fix-nav".chars() {
            app.rename_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            app.rename_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Event::Rename { id: 2, name: "fix-nav".into() })
        );
        assert!(!app.renaming());

        // ctrl+r inside a view renames the agent it shows, and the fleet comes back so the prompt is visible.
        click_first_agent(&mut app);
        assert!(app.in_worker_view());
        app.start_rename();
        assert!(!app.in_worker_view(), "the prompt needs the fleet's composer line");
        assert_eq!(app.rename.as_ref().map(|rename| rename.id), Some(2));

        // Esc drops what was typed and leaves the name alone.
        app.start_rename();
        app.rename_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.rename_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), None);
        assert!(!app.renaming());
        assert_eq!(app.label(2), names::label(2, None), "no event was sent, so no name changed");
    }

    #[test]
    fn no_cell_of_the_composer_is_underlined() {
        let mut app = App::new("m");
        app.insert("nothing underlined here");
        let buffer = drawn(&mut app, 60, 24);
        let underlined: Vec<(u16, u16)> = (0..24_u16)
            .flat_map(|y| (0..60_u16).map(move |x| (x, y)))
            .filter(|&(x, y)| buffer.cell((x, y)).is_some_and(|cell| cell.modifier.contains(Modifier::UNDERLINED)))
            .collect();
        assert!(underlined.is_empty(), "the composer draws no underline, found {underlined:?}");
    }

    #[test]
    fn the_caret_is_the_one_reversed_cell() {
        let mut app = App::new("m");
        app.composer.set_lines(vec!["hello".to_string()], (0, 2));
        let buffer = drawn(&mut app, 60, 24);
        let reversed: Vec<(u16, u16, String)> = (0..24_u16)
            .flat_map(|y| (0..60_u16).map(move |x| (x, y)))
            .filter_map(|(x, y)| {
                let cell = buffer.cell((x, y)).filter(|cell| cell.modifier.contains(Modifier::REVERSED))?;
                Some((x, y, cell.symbol().to_string()))
            })
            .collect();
        assert_eq!(reversed.len(), 1, "one cell is the caret: {reversed:?}");
        assert_eq!(
            reversed.first().map(|(_, _, symbol)| symbol.as_str()),
            Some("l"),
            "the caret sits on the character under it: {reversed:?}"
        );
    }

    #[test]
    fn an_agents_stream_is_its_own_history_in_order() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "run the tests")]));
        app.apply(UiEvent::Instruction { id: 2, text: "run the tests".into() });
        app.apply(UiEvent::AgentThought { id: 2, text: "first the build".into() });
        app.apply(UiEvent::AgentDelta { id: 2, text: "building".into() });
        let started = Progress::Started { call: 0, tool: tools::BASH.into(), summary: "cargo build".into() };
        app.apply(UiEvent::Progress { id: 2, progress: started });
        let preview = tools::preview(tools::BASH, "exit code: 0\nstdout:\n");
        let finished = Progress::Finished { call: 0, secs: 1.0, preview };
        app.apply(UiEvent::Progress { id: 2, progress: finished });
        app.apply(UiEvent::Boundary { id: 2 });
        app.apply(UiEvent::AgentDelta { id: 2, text: "green".into() });
        app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Finished, text: "green".into() });
        let got: Vec<(Role, &str)> =
            worker_entries(&app, 2).iter().map(|entry| (entry.role, entry.body.as_str())).collect();
        assert_eq!(
            got,
            [
                (Role::You, "run the tests"),
                (Role::Thinking, "first the build"),
                (Role::Model, "building"),
                (Role::Tool, "cargo build"),
                (Role::Model, "green"),
            ],
            "one entry per piece, in the order the agent produced them, and the answer once"
        );
        assert_eq!(app.unread_of(2), 5, "each new entry counts as one line the view has not shown");
        click_first_agent(&mut app);
        let view = screen(&mut app, 100, 24);
        assert!(view.contains("✻ first the build"), "the reasoning is marked as reasoning: {view}");
        let thought = view.find("first the build").expect("the thought is on screen");
        let card = view.find("$ cargo build").expect("the tool card is on screen");
        let answer = view.find("green").expect("the answer is on screen");
        assert!(thought < card && card < answer, "the screen keeps the order the call produced: {view}");
    }

    #[test]
    fn the_task_is_the_first_line_of_a_workers_stream_and_nowhere_else() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "fix the nav")]));
        app.apply(UiEvent::Instruction { id: 2, text: "fix the nav".into() });
        let fleet = screen(&mut app, 100, 24);
        assert!(!fleet.contains("fix the nav"), "the roster names the agent, not its prompt: {fleet}");
        click_first_agent(&mut app);
        let view = screen(&mut app, 100, 24);
        assert_eq!(view.matches("fix the nav").count(), 1, "said once, where it was said: {view}");
    }

    #[test]
    fn a_dead_agent_shows_as_failed() {
        let mut app = App::new("m");
        let mut dead = agent_view(2, "long job");
        dead.state = State::Failed;
        dead.busy_for = Some(Duration::from_secs(3));
        app.apply(UiEvent::Agents(vec![dead]));
        let screen = screen(&mut app, 100, 24);
        assert!(screen.contains("failed"), "{screen}");
        assert!(!screen.contains("working 3s"), "{screen}");
    }

    #[test]
    fn multiline_messages_keep_their_breaks() {
        let mut app = App::new("m");
        app.apply(UiEvent::Answer("first line\nsecond line".into()));
        let screen = screen(&mut app, 60, 16);
        assert!(screen.lines().any(|row| row.contains("first line")), "{screen}");
        assert!(screen.lines().any(|row| row.contains("second line")), "{screen}");
        assert!(!screen.contains("first line second line"), "{screen}");
    }

    #[test]
    fn submitted_prompt_clears_the_composer_and_reaches_history() {
        let mut app = App::new("m");
        app.insert("hello");
        assert_eq!(app.take_prompt(), "hello");
        assert!(app.composer.lines().iter().all(String::is_empty));
        app.recall_previous();
        assert_eq!(app.composer.lines(), ["hello"]);
    }

    #[test]
    fn a_new_entry_keeps_the_flattened_prefix() {
        let mut app = App::new("m");
        app.apply(UiEvent::Answer("first".into()));
        screen(&mut app, 60, 16);
        let prefix = app.lead.transcript.clone();
        assert_eq!(app.lead.dirty_from, app.lead.entries.len(), "a drawn transcript has nothing left to flatten");
        app.apply(UiEvent::Answer("second".into()));
        assert_eq!(app.lead.dirty_from, 1, "only the new entry needs flattening");
        screen(&mut app, 60, 16);
        assert_eq!(app.lead.transcript.get(..prefix.len()), Some(prefix.as_slice()));
        assert!(row_text(&app.lead.transcript).contains("second"));
    }

    #[test]
    fn finishing_an_early_tool_call_reflattens_from_its_entry() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, "bash", "ls");
        app.apply(UiEvent::Answer("after".into()));
        screen(&mut app, 60, 16);
        tool_result(&mut app, 0, "bash", "done", 1.0);
        app.toggle_tool_output();
        assert_eq!(app.lead.dirty_from, 0);
        let rendered = screen(&mut app, 60, 16);
        assert!(rendered.contains("done"), "{rendered}");
    }

    /// The entries of one worker's own stream.
    fn worker_entries(app: &App, id: usize) -> &[Entry] {
        app.worker_streams.iter().find(|stream| stream.id == id).map_or(&[], |worker| worker.stream.entries.as_slice())
    }

    /// One worker's stream, for its scroll position.
    fn stream_of(app: &App, id: usize) -> &Stream {
        &app.worker_streams.iter().find(|stream| stream.id == id).expect("the worker has a stream").stream
    }

    fn agent_view(id: usize, task: &str) -> AgentView {
        AgentView {
            id,
            name: names::name(id, None),
            label: names::label(id, None),
            parent: Some(Swarm::ROOT),
            depth: 1,
            dir: PathBuf::from("/tmp/repo"),
            task: task.into(),
            profile: String::new(),
            supervisor: false,
            state: State::Idle,
            read_only: false,
            busy_for: None,
        }
    }

    /// The root's streamed text, which lands in the transcript.
    fn delta(app: &mut App, text: &str) {
        app.apply(UiEvent::AgentDelta { id: Swarm::ROOT, text: text.into() });
    }

    /// One tool call in the transcript, started and finished.
    fn tool_call(app: &mut App, call: usize, tool: &str, summary: &str) {
        let progress = Progress::Started { call, tool: tool.into(), summary: summary.into() };
        app.apply(UiEvent::Progress { id: Swarm::ROOT, progress });
    }

    fn tool_result(app: &mut App, call: usize, tool: &str, output: &str, secs: f64) {
        let progress = Progress::Finished { call, secs, preview: tools::preview(tool, output) };
        app.apply(UiEvent::Progress { id: Swarm::ROOT, progress });
    }

    /// Clicks the first agent's block in the panel that is not the root, the way the mouse does.
    fn click_first_agent(app: &mut App) {
        screen(app, 100, 24);
        let (top, _, id) =
            *app.worker_rows.iter().find(|(_, _, id)| *id != Swarm::ROOT).expect("the panel draws a block per agent");
        app.click(app.workers_area.x + 2, top);
        assert_eq!(app.selected, Some(id), "the click picked that agent");
    }

    /// The root's own view, which the panel always draws first.
    fn root_view() -> AgentView {
        let mut root = agent_view(Swarm::ROOT, "");
        root.parent = None;
        root.depth = 0;
        root
    }

    #[test]
    fn a_click_opens_the_agent_view_and_esc_returns_to_the_tree() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(3, "fix the nav")]));
        app.apply(UiEvent::Instruction { id: 3, text: "fix the nav".into() });
        app.apply(UiEvent::Agent { id: 3, kind: AgentEvent::Finished, text: "done".into() });
        click_first_agent(&mut app);
        assert!(app.in_worker_view());
        let view = screen(&mut app, 100, 24);
        assert!(view.contains(&format!("agents › {}", names::label(3, None))), "{view}");
        assert!(view.contains("fix the nav"), "the stream opens with the instruction: {view}");
        assert!(view.contains("esc back to agents"), "the way out is on the header: {view}");
        assert!(
            view.contains(&format!("[agent 3 · {}] done done", names::name(3, None))),
            "the view shows the agent's own lines: {view}"
        );
        assert!(!view.contains("Ask agent 1"), "the composer is out of the way: {view}");
        app.worker_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.in_worker_view(), "esc returns to the tree");
        assert!(!app.quit, "esc never quits the app");
        assert_eq!(app.selected, Some(3), "the tree keeps the agent picked");
        let list = screen(&mut app, 100, 24);
        assert!(list.contains("Ask agent 1"), "{list}");
        assert!(list.contains("agent 3"), "the transcript still names the agent it kept picked: {list}");
    }

    #[test]
    fn the_worker_panel_names_the_profile() {
        let mut app = App::new("m");
        let mut reviewer = agent_view(2, "review");
        reviewer.profile = "reviewer".into();
        app.apply(UiEvent::Agents(vec![root_view(), reviewer]));
        let screen = screen(&mut app, 120, 30);
        assert!(screen.contains("(2) reviewer"), "the panel shows the flavor: {screen}");
    }

    /// The rule between the panes is a handle: a press on it, then a drag, sets the panel's width.
    #[test]
    fn a_drag_of_the_rule_resizes_the_agent_panel() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "job")]));
        screen(&mut app, 120, 24);
        let opened = app.workers_area.width;
        let rule = app.split.expect("the panel draws its rule");
        assert_eq!(rule + WORKERS_WIDTH, 120, "the rule sits where the panel begins");
        app.click(rule, 1);
        app.drag(rule - 20);
        screen(&mut app, 120, 24);
        assert_eq!(app.workers_area.width, opened + 20, "the panel followed the pointer left");
        assert_eq!(app.split, Some(rule - 20), "and its rule moved with it");
        // Dragged past the far edge: the transcript keeps its columns, so the panel stops growing.
        app.drag(0);
        screen(&mut app, 120, 24);
        assert_eq!(app.workers_area.width + 2, 120 - TRANSCRIPT_MIN, "the transcript is never squeezed away");
        // Dragged past the near edge: the panel stops at its narrowest.
        app.drag(120);
        screen(&mut app, 120, 24);
        assert_eq!(app.workers_area.width + 2, WORKERS_MIN, "the panel never vanishes");
        app.release();
        app.click(120, 1);
        assert_eq!(app.workers_area.width + 2, WORKERS_MIN, "a click past the panel resizes nothing");
    }

    /// The width a drag lands on is what the next run opens at, and no drag can hide the panel.
    #[test]
    fn a_resize_is_written_down_once_and_narrowed_no_further_than_the_floor() {
        let mut app = App::new("m");
        screen(&mut app, 120, 24);
        assert!(!app.take_width_dirty(), "a fresh app has nothing to write down");
        let rule = app.split.expect("the panel draws its rule");
        app.click(rule, 1);
        app.drag(rule - 10);
        assert!(app.take_width_dirty(), "the drag asks for one write");
        assert!(!app.take_width_dirty(), "and only one, however many frames it took");
        screen(&mut app, 120, 24);
        let mut next = App::new("m");
        next.set_workers_width(app.workers_width());
        screen(&mut next, 120, 24);
        assert_eq!(next.workers_area, app.workers_area, "the next run opens where the drag left the panel");
        next.set_workers_width(0);
        assert_eq!(next.workers_width(), WORKERS_MIN, "a width that would hide the panel is refused");
    }

    /// A drag starts on the rule and nowhere else: nothing else in the frame moves the split.
    #[test]
    fn a_drag_off_the_rule_leaves_the_panel_alone() {
        let mut app = App::new("m");
        screen(&mut app, 120, 24);
        let before = app.workers_width;
        app.drag(10);
        assert_eq!(app.workers_width, before, "no press on the rule, no resize");
        let rule = app.split.expect("the panel draws its rule");
        app.click(rule, 1);
        app.drag(rule - 10);
        assert_ne!(app.workers_width, before, "the press on the rule is what arms the drag");
    }

    #[test]
    fn clicking_the_back_line_returns_to_the_tree() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "job")]));
        click_first_agent(&mut app);
        screen(&mut app, 100, 24);
        let back = app.back_area;
        app.click(back.x, back.y);
        assert!(!app.in_worker_view(), "the back line is the mouse way out");
    }

    #[test]
    fn down_and_up_move_the_view_between_agents() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "a"), agent_view(3, "b")]));
        app.toggle_worker_view();
        assert_eq!(app.selected, Some(Swarm::ROOT), "F4 starts at the root");
        app.worker_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.selected, Some(2), "down is the next agent");
        app.worker_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.selected, Some(3), "down again is the deepest");
        app.worker_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.selected, Some(3), "the last agent is the end of the list");
        app.worker_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.selected, Some(2), "up is the previous agent");
        assert!(app.in_worker_view(), "moving between agents stays in the view");
    }

    #[test]
    fn f4_opens_and_closes_an_agent_view() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "a"), agent_view(3, "b")]));
        app.toggle_worker_view();
        assert!(app.in_worker_view());
        assert_eq!(app.selected, Some(Swarm::ROOT), "F4 picks the first agent when none is picked");
        app.toggle_worker_view();
        assert!(!app.in_worker_view());
        app.toggle_worker_view();
        assert_eq!(app.selected, Some(Swarm::ROOT), "F4 reopens the agent still picked");
        assert!(app.in_worker_view());
    }

    #[test]
    fn f4_with_no_agents_says_so_and_opens_nothing() {
        let mut app = App::new("m");
        app.toggle_worker_view();
        assert!(!app.in_worker_view());
        assert!(row_text(&app.lead.entries[0].lines(80, 0, true)).contains("no agents yet"));
    }

    #[test]
    fn a_click_outside_the_agent_panel_opens_nothing() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "job")]));
        screen(&mut app, 100, 24);
        app.click(1, 1);
        assert_eq!(app.selected, None);
        assert!(!app.in_worker_view());
    }

    #[test]
    fn an_agent_event_reaches_its_own_stream_and_one_lead_lifecycle_line() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(3, "fix the nav")]));
        app.apply(UiEvent::Agent { id: 3, kind: AgentEvent::Reply, text: "halfway\nsecond line".into() });
        app.apply(UiEvent::Agent { id: 3, kind: AgentEvent::Finished, text: "tests pass".into() });
        let stream = worker_entries(&app, 3);
        assert_eq!(stream.len(), 2, "the agent's stream keeps one entry per event");
        assert_eq!(stream[0].body, "halfway\nsecond line", "and the whole message, lines and all");
        let lead: Vec<&str> = app.lead.entries.iter().map(|entry| entry.body.as_str()).collect();
        assert_eq!(lead.len(), 3, "one started line and one per event class: {lead:?}");
        assert!(lead[0].contains("started"), "{lead:?}");
        assert!(lead[1].ends_with("says: halfway"), "{lead:?}");
        assert!(lead[2].ends_with("finished: tests pass"), "{lead:?}");
        assert!(!lead.iter().any(|line| line.contains("second line")), "no message body leaks: {lead:?}");
    }

    #[test]
    fn unread_counts_what_arrived_while_the_view_was_closed_and_clears_on_open() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "a"), agent_view(3, "b")]));
        app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Finished, text: "done".into() });
        assert_eq!(app.unread_of(2), 1);
        assert_eq!(app.unread_of(3), 0);
        let panel = screen(&mut app, 100, 24);
        assert!(panel.contains("· 1 new"), "the list counts what was missed: {panel}");
        click_first_agent(&mut app);
        assert_eq!(app.unread_of(2), 0, "opening the view clears its counter");
        app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Reply, text: "more".into() });
        assert_eq!(app.unread_of(2), 0, "what the open view shows is read");
        app.apply(UiEvent::Agent { id: 3, kind: AgentEvent::Finished, text: "b done".into() });
        assert_eq!(app.unread_of(3), 1, "another agent's lines still count");
    }

    #[test]
    fn each_view_keeps_its_own_buffer_and_scroll_position() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "a")]));
        for index in 0..40 {
            app.apply(UiEvent::Answer(format!("lead line {index}")));
        }
        for index in 0..40 {
            app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Reply, text: format!("agent line {index}") });
        }
        screen(&mut app, 80, 16);
        app.scroll_up();
        assert!(!app.lead.follow, "the lead stops following when scrolled up");
        let lead_scroll = app.lead.scroll;
        assert!(lead_scroll > 0, "the lead has somewhere to scroll");
        app.toggle_worker_view();
        app.worker_key(KeyEvent::from(KeyCode::Down));
        screen(&mut app, 80, 16);
        app.scroll_top();
        assert!(!stream_of(&app, 2).follow, "the agent's stream has its own tail state");
        app.leave_worker();
        screen(&mut app, 80, 16);
        assert_eq!(app.lead.scroll, lead_scroll, "the lead kept its position");
        assert!(!app.lead.follow);
        app.toggle_worker_view();
        screen(&mut app, 80, 16);
        assert!(!stream_of(&app, 2).follow, "and the agent kept its own");
    }

    #[test]
    fn an_agent_view_shows_only_that_agents_stream() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(2, "a"), agent_view(3, "b")]));
        app.apply(UiEvent::Answer("lead only line".into()));
        app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Finished, text: "agent one line".into() });
        app.apply(UiEvent::Agent { id: 3, kind: AgentEvent::Finished, text: "agent two line".into() });
        click_first_agent(&mut app);
        let view = screen(&mut app, 100, 24);
        assert!(view.contains("agent one line"), "{view}");
        assert!(!view.contains("agent two line"), "another agent's stream stays out: {view}");
        assert!(!view.contains("lead only line"), "the lead's stream stays out: {view}");
        app.leave_worker();
        let lead = screen(&mut app, 100, 24);
        assert!(lead.contains("lead only line"), "{lead}");
        assert_eq!(lead.matches("agent one line").count(), 1, "one lifecycle line, not the message entry: {lead}");
        assert!(lead.contains("finished: agent one line"), "{lead}");
    }

    /// A PNG header with a pixel size, so the chip has something to read.
    fn png(dir: &Path, name: &str, width: u32, height: u32) -> PathBuf {
        let path = dir.join(name);
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        std::fs::write(&path, &bytes).unwrap();
        path
    }

    #[test]
    fn the_chip_shows_the_paste_in_the_tick_the_key_lands_and_what_it_became() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new("m");
        let id = app.begin_paste().expect("a paste");
        let pending = screen(&mut app, 80, 24);
        assert!(pending.contains("reading the clipboard"), "the chip is up before the read: {pending}");
        app.attach(id, clipboard::Paste::Image(png(dir.path(), "shot.png", 64, 32)));
        let ready = screen(&mut app, 80, 24);
        assert!(ready.contains("shot.png"), "{ready}");
        assert!(ready.contains("64×32"), "the chip shows the pixels: {ready}");
        assert!(ready.contains("✓ attached"), "and a verdict that is not colour alone: {ready}");
        assert!(ready.contains("ctrl+x removes"), "{ready}");
    }

    #[test]
    fn a_paste_that_found_no_image_says_so_in_the_chip() {
        let mut app = App::new("m");
        let id = app.begin_paste().expect("a paste");
        app.attach(id, clipboard::Paste::Empty);
        let failed = screen(&mut app, 80, 24);
        assert!(failed.contains("✗"), "the chip carries a glyph, not just a colour: {failed}");
        assert!(failed.contains(NO_CLIPBOARD), "{failed}");
        assert!(failed.contains("ctrl+x dismisses"), "{failed}");
    }

    #[test]
    fn a_narrow_chip_cuts_the_name_and_keeps_the_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new("m");
        let id = app.begin_paste().expect("a paste");
        let long = png(dir.path(), &"a-very-long-screenshot-name".repeat(4), 2560, 1440);
        app.attach(id, clipboard::Paste::Image(long));
        let narrow = screen(&mut app, 40, 24);
        for row in narrow.lines() {
            assert!(row.chars().count() <= 40, "the chip never wraps: {row:?}");
        }
        assert!(narrow.contains("· ✓ attached"), "the verdict survives a narrow line: {narrow}");
        assert!(narrow.contains('…'), "a long name is what gets cut: {narrow}");
        let wide = screen(&mut app, 100, 24);
        assert!(wide.contains("ctrl+x removes"), "a wide chip keeps the way out on the line: {wide}");
    }

    #[test]
    fn a_turn_after_ctrl_d_cancels_the_drain() {
        let mut app = App::new("m");
        app.drain();
        assert!(app.draining);
        app.apply(UiEvent::Busy(true));
        assert!(!app.draining);
    }

    #[test]
    fn every_entry_is_a_block_of_its_own_and_only_two_thoughts_touch() {
        let mut app = App::new("m");
        app.apply(UiEvent::Answer("on it".into()));
        tool_call(&mut app, 0, "bash", "ls");
        screen(&mut app, 60, 16);
        let rows: Vec<String> = app.lead.transcript.iter().map(|line| row_text(std::slice::from_ref(line))).collect();
        assert_eq!(rows.len(), 3, "one blank row keeps the call off the message it answers: {rows:?}");
        assert!(rows[0].contains("on it"), "{rows:?}");
        assert!(rows[1].is_empty(), "{rows:?}");
        assert!(rows[2].contains("$ ls"), "{rows:?}");
        app.apply(UiEvent::Notice("agent 2 is idle".into()));
        screen(&mut app, 60, 16);
        let rows: Vec<String> = app.lead.transcript.iter().map(|line| row_text(std::slice::from_ref(line))).collect();
        assert_eq!(rows.len(), 5, "a notice opens a paragraph of its own: {rows:?}");
        assert!(rows[3].is_empty() && rows[4].contains("agent 2 is idle"), "{rows:?}");

        // The reasoning is the one pair that runs on: two thoughts of one answer read as one region.
        let mut app = App::new("m");
        for (role, body) in
            [(Role::You, "go"), (Role::Thinking, "first"), (Role::Thinking, "second"), (Role::Model, "done")]
        {
            app.lead.push(Entry::new(role, String::new(), body.into()));
        }
        screen(&mut app, 60, 16);
        let rows: Vec<String> = app.lead.transcript.iter().map(|line| row_text(std::slice::from_ref(line))).collect();
        let first = rows.iter().position(|row| row.contains("first")).expect("the first thought is on screen");
        let second = rows.iter().position(|row| row.contains("second")).expect("the second one too");
        assert_eq!(second, first + 1, "the thoughts touch: {rows:?}");
        assert!(rows.last().is_some_and(|row| row.contains("done")), "the answer is a block apart: {rows:?}");
    }

    #[test]
    fn an_answer_is_a_bubble_pushed_right_and_the_users_words_one_flush_left() {
        let mut app = App::new("m");
        app.echo("the question");
        app.apply(UiEvent::Answer("the answer".into()));
        app.toggle_workers();
        let buffer = drawn(&mut app, 80, 24);
        let cell = |x: u16, y: u16| buffer.cell((x, y));
        let fill = |x: u16, y: u16| cell(x, y).map(|cell| cell.bg);
        let rows: Vec<String> =
            (0..24).map(|y| (0..80).map(|x| cell(x, y).map_or(" ", ratatui::buffer::Cell::symbol)).collect()).collect();
        let at = |needle: &str| {
            let row = rows.iter().position(|row| row.contains(needle)).expect("the line is on screen");
            u16::try_from(row).expect("24 rows fit a u16")
        };
        let (question, answer) = (at("the question"), at("the answer"));
        // The transcript insets two columns each side: the user's bubble starts on the left one, the
        // answer's ends on the right one, and neither crosses to the other side.
        assert_eq!(fill(2, question), Some(theme::you_fill()), "{}", rows[usize::from(question)]);
        assert_eq!(fill(77, answer), Some(theme::agent_fill()), "{}", rows[usize::from(answer)]);
        assert_ne!(fill(2, answer), Some(theme::agent_fill()), "{}", rows[usize::from(answer)]);
        // The composer shares the fill of what the user typed.
        assert_eq!(fill(0, 20), Some(theme::you_fill()), "the composer is filled");
        assert_eq!(fill(79, 20), Some(theme::you_fill()));
    }

    #[test]
    fn f5_hands_the_mouse_back_to_the_terminal_and_captures_it_again() {
        let mut app = App::new("m");
        assert!(app.mouse, "the mouse is captured at startup");
        assert!(!app.toggle_mouse());
        let said = row_text(&app.lead.entries[0].lines(200, 0, true));
        assert!(said.contains("select and copy with the terminal"), "{said}");
        assert!(app.toggle_mouse(), "F5 captures it again");
        assert!(app.mouse);
    }

    #[test]
    fn f5_in_an_agent_view_says_so_there_and_not_behind_it() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![agent_view(2, "a")]));
        app.toggle_worker_view();
        let lead = app.lead.entries.len();
        assert!(!app.toggle_mouse());
        assert_eq!(app.lead.entries.len(), lead, "the lead's stream stays out of sight while a view is open");
        let said = worker_entries(&app, 2).last().expect("the view says what F5 did");
        assert!(row_text(&said.lines(200, 0, true)).contains("select and copy with the terminal"));
    }

    #[test]
    fn a_paste_never_fills_a_prompt_nobody_can_see() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![agent_view(2, "a")]));
        app.toggle_worker_view();
        app.paste("a pasted sentence");
        assert!(app.take_prompt().is_empty(), "a paste in a worker view never fills the hidden prompt");
        let last = app.lead.entries.last().expect("a notice says where the text would go");
        assert!(row_text(&last.lines(200, 0, true)).contains("esc leaves this view first"));
        app.leave_worker();
        app.paste("a pasted sentence");
        assert_eq!(app.take_prompt(), "a pasted sentence", "the prompt takes a paste once the view is closed");
    }

    /// Types one key the way the loop does: through the composer's own input path.
    fn type_key(app: &mut App, code: KeyCode) {
        app.input(KeyEvent::from(code));
    }

    /// The rows the popup would draw, as the text each one reads as.
    fn menu_rows(app: &App) -> Vec<String> {
        app.completion.as_ref().map_or_else(Vec::new, |menu| {
            menu.candidates.iter().map(|candidate| row_text(&[candidate.line()])).collect()
        })
    }

    #[test]
    fn the_popup_narrows_to_the_token_and_a_space_ends_it() {
        let mut app = App::new("m");
        assert!(!app.completing(), "an empty composer offers nothing");
        type_key(&mut app, KeyCode::Char('/'));
        let rows = menu_rows(&app);
        assert_eq!(rows.len(), slash::COMMANDS.len(), "/ offers every command: {rows:?}");
        assert!(rows[0].starts_with("/resume <id>"), "{rows:?}");
        assert!(rows[1].starts_with("/pipeline <name|list|stop>"), "{rows:?}");
        assert!(rows[4].starts_with("/approve <server>"), "the table's order is the popup's: {rows:?}");
        assert!(rows[5].starts_with("/login <server>"), "the table's order is the popup's: {rows:?}");
        type_key(&mut app, KeyCode::Char('m'));
        assert_eq!(menu_rows(&app), ["/mcp  show the MCP servers"], "only what was typed is offered");
        type_key(&mut app, KeyCode::Char(' '));
        assert!(!app.completing(), "a space ends the command: there is nothing left to complete");
        assert_eq!(app.composer.cursor(), (0, 3), "and it reached the composer");
        type_key(&mut app, KeyCode::Char('x'));
        assert!(!app.completing(), "an argument is no command name");
    }

    #[test]
    fn accepting_a_command_leaves_the_name_its_argument_follows_and_the_caret_at_the_end() {
        let mut app = App::new("m");
        for key in [KeyCode::Char('/'), KeyCode::Char('s'), KeyCode::Char('c')] {
            type_key(&mut app, key);
        }
        assert!(app.accept_completion(), "the popup has a row to accept");
        assert!(!app.completing(), "the popup closes on the row it accepted");
        assert_eq!(app.composer.cursor(), (0, 8), "the caret sits at the end of the command");
        assert_eq!(app.take_prompt(), "/schemes", "a command with no argument lands bare");

        let mut app = App::new("m");
        for key in [KeyCode::Char('/'), KeyCode::Char('r'), KeyCode::Char('e')] {
            type_key(&mut app, key);
        }
        assert!(app.accept_completion());
        assert_eq!(app.composer.cursor(), (0, 8), "the caret waits where the argument goes");
        assert_eq!(
            app.composer.lines().join("\n"),
            "/resume ",
            "a command with an argument leaves the space its argument follows in"
        );
    }

    #[test]
    fn closing_the_popup_keeps_what_was_typed() {
        let mut app = App::new("m");
        type_key(&mut app, KeyCode::Char('/'));
        type_key(&mut app, KeyCode::Char('s'));
        assert!(app.completing());
        app.close_completion();
        assert!(!app.completing(), "esc closes the popup");
        assert_eq!(app.take_prompt(), "/s", "and leaves the keys the user typed where they are");
    }

    #[test]
    fn resume_completes_the_agent_ids_the_panel_shows() {
        let mut app = App::new("m");
        let mut stopped = agent_view(4, "gone quiet");
        stopped.state = State::Stopped;
        stopped.dir = PathBuf::from("/tmp/other");
        app.apply(UiEvent::Agents(vec![root_view(), agent_view(3, "fix the nav"), stopped]));
        app.paste("/resume ");
        let rows = menu_rows(&app);
        assert_eq!(
            rows,
            ["1  idle  /tmp/repo", "3  idle  /tmp/repo", "4  stopped  /tmp/other"],
            "every agent the panel draws is offered, with its state and where it sits"
        );
        app.step_completion(true);
        assert_eq!(app.completion.as_ref().map(|menu| menu.selected), Some(1), "down moves the highlight");
        app.step_completion(false);
        assert_eq!(app.completion.as_ref().map(|menu| menu.selected), Some(0), "up moves it back");
        assert_eq!(app.composer.cursor(), (0, 8), "and neither moves the caret");
        app.paste("3");
        assert_eq!(menu_rows(&app), ["3  idle  /tmp/repo"], "a partial id narrows the list");
        assert!(app.accept_completion());
        assert_eq!(app.take_prompt(), "/resume 3", "accepting an id writes the command and the id");
    }

    #[test]
    fn typing_a_slash_lists_the_commands_above_the_composer_and_the_popup_goes_on_esc() {
        let mut app = App::new("m");
        type_key(&mut app, KeyCode::Char('/'));
        let open = screen(&mut app, 100, 24);
        assert!(open.contains("/resume <id>"), "{open}");
        assert!(open.contains("retask an agent without a model call"), "{open}");
        assert!(open.contains("/login <server>"), "{open}");
        app.close_completion();
        let closed = screen(&mut app, 100, 24);
        assert!(!closed.contains("/resume <id>"), "the rows are gone: {closed}");
        assert!(!closed.contains("/schemes"), "{closed}");
        assert!(closed.contains("┃"), "the composer is still there: {closed}");
    }

    /// The picked row is the one wearing the band: a menu whose highlight you cannot see is a menu
    /// you cannot steer.
    #[test]
    fn the_highlighted_row_is_the_only_one_wearing_the_band() {
        let mut app = App::new("m");
        type_key(&mut app, KeyCode::Char('/'));
        let buffer = drawn(&mut app, 100, 24);
        let band = theme::band().bg;
        let mut banded = Vec::new();
        for y in 0..24 {
            let row: String = (0..100).filter_map(|x| buffer.cell((x, y))).map(ratatui::buffer::Cell::symbol).collect();
            if row.contains("/resume <id>") || row.contains("/schemes") {
                let cell = buffer.cell((2, y)).expect("the popup's first text column");
                banded.push(Some(cell.bg) == band);
            }
        }
        assert_eq!(banded, [true, false], "the first row is picked and the one under it is not");
    }
}
