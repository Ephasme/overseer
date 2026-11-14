use super::theme;
use crate::clipboard;
use crate::event::UiEvent;
use crate::swarm::{AgentEvent, AgentView, Progress, Swarm};
use crate::tools;
use crate::version;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthChar;

/// Frames of the busy spinner.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Shortest call worth timing: below it the number is noise, not information.
const TIMED: f64 = 0.1;

/// Rows the composer grows to before it scrolls.
const COMPOSER_ROWS: u16 = 8;

/// Width of the worker panel, its rule included.
const WORKERS_WIDTH: u16 = 34;

/// Rows one wheel notch scrolls.
const WHEEL_LINES: u16 = 3;

/// Redraw interval of the spinner.
const TICK: Duration = Duration::from_millis(120);

/// Longest worker preview kept, in characters.
const PREVIEW_CHARS: usize = 400;

/// Body rows a collapsed tool card keeps, before the row that counts what it hid.
const BODY_ROWS: usize = 2;

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

/// Key hints, most useful first: a narrow terminal drops the tail rather than the first ones.
const HINTS: [&str; 8] =
    ["enter send", "ctrl+j newline", "F2 workers", "F3 expand", "ctrl+o expand", "F4 view", "F5 mouse", "ctrl+d quit"];

/// Key hints while a worker's view is open: the way out is on its own header line.
const WORKER_HINTS: [&str; 3] = ["F4 toggle", "F5 mouse", "ctrl+d quit"];

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
    You,
    Overseer,
    /// A tool call, by the overseer or by one of its workers.
    Tool,
    Worker(AgentEvent),
    Notice,
    Error,
    Debug,
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
        self.output = Some("(no result: its worker stopped)".into());
        self.failed = true;
    }

    fn lines(&self, width: u16, spinning: usize, expand: bool) -> Vec<Line<'static>> {
        let width = usize::from(width).max(8);
        match self.role {
            Role::You => {
                bar("▌ ", Style::new().fg(theme::primary()), &self.body, Style::new().fg(theme::text()), width)
            }
            // The overseer reads as plain prose: the user's bar above it is the only mark it needs.
            Role::Overseer => block("  ", Style::new(), &self.body, Style::new().fg(theme::text()), width),
            Role::Tool => self.tool_lines(width, spinning, expand),
            Role::Worker(kind) => {
                let (label, colour) = match kind {
                    AgentEvent::Reply => ("says", theme::info()),
                    AgentEvent::Finished => ("done", theme::success()),
                    AgentEvent::Failed => ("failed", theme::error()),
                    AgentEvent::Interrupted => ("interrupted", theme::warning()),
                };
                let prefix = format!("[{}] {label} ", self.title);
                block(&prefix, Style::new().fg(colour), &self.body, Style::new().fg(theme::muted()), width)
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

    /// A tool call: a header naming what it acts on, then the head of what it returned.
    fn tool_lines(&self, width: usize, spinning: usize, expand: bool) -> Vec<Line<'static>> {
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
        // The body, wrapped: a collapsed card keeps its first rows, an expanded one shows it whole.
        let mut rows: Vec<String> = Vec::new();
        for source in self.body.split('\n') {
            rows.extend(wrap(source, room));
        }
        let hidden = if expand { 0 } else { rows.len().saturating_sub(BODY_ROWS) };
        rows.truncate(rows.len() - hidden);
        let pad = " ".repeat(lead + text_width(verb) + 1);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            if index > 0 {
                lines.push(Line::from(vec![
                    Span::styled(pad.clone(), Style::new()),
                    Span::styled(row, Style::new().fg(theme::text())),
                ]));
                continue;
            }
            let mut spans = vec![
                Span::styled(format!("{glyph} "), Style::new().fg(colour)),
                Span::styled(verb.to_string(), Style::new().fg(theme::muted())),
            ];
            if !row.is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(row, Style::new().fg(theme::text())));
            }
            spans.push(Span::styled(chips.clone(), Style::new().fg(theme::faint())));
            lines.push(Line::from(spans));
        }
        if hidden > 0 {
            let plural = if hidden == 1 { "line" } else { "lines" };
            lines.push(Line::from(Span::styled(
                format!("{pad}… {hidden} more {plural}"),
                Style::new().fg(theme::faint()),
            )));
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
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::new().fg(theme::rule())),
                Span::styled(row.clone(), style),
            ]));
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
}

/// How a tool call reads: a shell command is `$`, the file tools say what they do.
fn verb(tool: &str) -> &str {
    match tool {
        tools::BASH => "$",
        tools::READ_FILE => "read",
        tools::WRITE_FILE => "write",
        other => other,
    }
}

/// A prefix on the first row, spaces of the same width on the rest, then the wrapped body indented.
fn block(prefix: &str, prefix_style: Style, body: &str, body_style: Style, width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(text_width(prefix)).max(8);
    let pad = " ".repeat(text_width(prefix));
    let mut lines = Vec::new();
    for (line, source) in body.split('\n').enumerate() {
        for (row, text) in wrap(source, room).into_iter().enumerate() {
            let lead = if line == 0 && row == 0 { prefix } else { pad.as_str() };
            let lead = Span::styled(lead.to_string(), prefix_style);
            lines.push(Line::from(vec![lead, Span::styled(text, body_style)]));
        }
    }
    lines
}

/// A quoted block: every row carries the bar, so a message of several lines reads as one region.
fn bar(prefix: &str, prefix_style: Style, body: &str, body_style: Style, width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(text_width(prefix)).max(8);
    let mut lines = Vec::new();
    for source in body.split('\n') {
        for text in wrap(source, room) {
            lines
                .push(Line::from(vec![Span::styled(prefix.to_string(), prefix_style), Span::styled(text, body_style)]));
        }
    }
    lines
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

    /// Blank rows before an entry: a tool call hangs off the message or call above it, with no gap.
    fn gap_before(&self, index: usize) -> usize {
        let previous = self.entries.get(index.wrapping_sub(1)).map(|entry| &entry.role);
        let current = self.entries.get(index).map(|entry| &entry.role);
        let attached = matches!((previous, current), (Some(Role::You | Role::Overseer | Role::Tool), Some(Role::Tool)));
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

/// The composer: one bar on the left, room to breathe, and a half block closing the bar.
fn composer() -> TextArea<'static> {
    let mut area = TextArea::default();
    area.set_block(
        Block::new()
            .borders(Borders::LEFT | Borders::BOTTOM)
            .border_set(PROMPT_FRAME)
            .border_style(Style::new().fg(theme::edge()))
            .padding(Padding::new(1, 1, 1, 0)),
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
    /// Where each agent tool call's entry landed: (agent, its call number, entry index).
    calls: Vec<(usize, usize, usize)>,
    /// Index of the entry the overseer's streamed text is filling.
    live: Option<usize>,
    workers: Vec<AgentView>,
    /// Last streamed text per worker, for the panel.
    previews: Vec<(usize, String)>,
    worker_panel: bool,
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
            model: model.to_string(),
            history: Vec::new(),
            recall: None,
            calls: Vec::new(),
            live: None,
            workers: Vec::new(),
            previews: Vec::new(),
            worker_panel: true,
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
            UiEvent::TurnBoundary => self.boundary(),
            UiEvent::Agent { id, kind, text } => {
                self.clear_preview(id);
                let name = format!("agent {id}");
                // The agent's own stream keeps the whole message; the lead gets one lifecycle line.
                let first = text.lines().next().unwrap_or_default().trim().to_string();
                let line = if first.is_empty() {
                    format!("{name} {}", kind.verb())
                } else {
                    format!("{name} {}: {first}", kind.verb())
                };
                self.worker_stream_mut(id).push(Entry::new(Role::Worker(kind), name, text));
                self.note_unread(id);
                self.push(Role::Notice, String::new(), line);
            }
            UiEvent::AgentDelta { id, text } => {
                if id == Swarm::ROOT {
                    self.stream(&text);
                } else {
                    self.preview(id, &text);
                }
            }
            UiEvent::Progress { id, progress } => self.progress(id, progress),
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
                    let line = format!("agent {id} started · {dir}");
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
                    // A failed turn never sends an answer: its streamed text stays as the record.
                    self.live = None;
                }
            }
            UiEvent::Done => self.quit = true,
        }
    }

    /// An agent's tool call: one entry that opens when it starts and closes when it returns.
    fn progress(&mut self, id: usize, progress: Progress) {
        match progress {
            Progress::Started { call, tool, summary } => {
                // The card ends the paragraph above it: what the next model call streams belongs
                // below the card, not glued into the entry it would otherwise still be writing to.
                if id == Swarm::ROOT {
                    self.live = None;
                }
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
        while let Some(slot) = self
            .calls
            .iter()
            .position(|(worker, _, _)| self.workers.iter().any(|view| view.id == *worker && view.finished))
        {
            let (worker, _, index) = self.calls.swap_remove(slot);
            let stream = self.worker_stream_mut(worker);
            if let Some(entry) = stream.entries.get_mut(index) {
                entry.abandon();
            }
            stream.invalidate(index);
        }
    }

    /// Separates the model calls of one turn with a blank line.
    fn boundary(&mut self) {
        let Some(index) = self.live else { return };
        let changed = if let Some(entry) = self.lead.entries.get_mut(index)
            && !entry.body.is_empty()
            && !entry.body.ends_with("\n\n")
        {
            entry.body.push_str("\n\n");
            true
        } else {
            false
        };
        if changed {
            self.lead.invalidate(index);
        }
    }

    fn stream(&mut self, text: &str) {
        if self.live.is_none() {
            self.push(Role::Overseer, String::new(), String::new());
            self.live = Some(self.lead.entries.len().saturating_sub(1));
        }
        if let Some(index) = self.live
            && let Some(entry) = self.lead.entries.get_mut(index)
        {
            entry.body.push_str(text);
            self.lead.invalidate(index);
        }
    }

    /// The turn's final text: it replaces what streamed when a backend sends nothing.
    fn answer(&mut self, text: String) {
        let live = self.live.take();
        match live.and_then(|index| self.lead.entries.get_mut(index)) {
            Some(entry) if entry.body.trim().is_empty() => entry.body = text,
            Some(entry) if !text.trim().is_empty() => {
                let streamed = entry.body.trim_end();
                let final_text = text.trim();
                if streamed.starts_with(final_text) {
                    // The streamed body already holds the final text, perhaps with more.
                } else if final_text.starts_with(streamed) {
                    // The streamed body was a prefix: replace it with the full text.
                    entry.body = text;
                } else if !streamed.ends_with(final_text) {
                    // Neither is a prefix of the other: append the final text.
                    entry.body.push_str("\n\n");
                    entry.body.push_str(&text);
                }
            }
            Some(_) => {}
            None => self.push(Role::Overseer, String::new(), text),
        }
        if let Some(index) = live {
            self.lead.invalidate(index);
        }
    }

    fn preview(&mut self, id: usize, text: &str) {
        self.dirty = true;
        let slot = self.previews.iter().position(|(worker, _)| *worker == id).unwrap_or_else(|| {
            self.previews.push((id, String::new()));
            self.previews.len().saturating_sub(1)
        });
        if let Some((_, buffer)) = self.previews.get_mut(slot) {
            buffer.push_str(text);
            let length = buffer.chars().count();
            if length > PREVIEW_CHARS {
                *buffer = buffer.chars().skip(length - PREVIEW_CHARS).collect();
            }
        }
    }

    fn clear_preview(&mut self, id: usize) {
        self.previews.retain(|(worker, _)| *worker != id);
    }

    fn preview_of(&self, id: usize) -> Option<&str> {
        self.previews.iter().find(|(worker, _)| *worker == id).map(|(_, text)| text.as_str())
    }

    /// A line in the lead's stream: the only stream the overseer's own events reach.
    fn push(&mut self, role: Role, title: String, body: String) {
        // Anything landing in the transcript closes the streamed paragraph, so no later delta is
        // appended above the line that interrupted it.
        self.live = None;
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
            0
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
        } else {
            let transcript = if self.worker_panel && body.width > WORKERS_WIDTH + 40 {
                let [left, right] =
                    Layout::horizontal([Constraint::Min(40), Constraint::Length(WORKERS_WIDTH)]).areas(body);
                self.render_workers(frame, right);
                left
            } else {
                self.workers_area = Rect::ZERO;
                self.worker_rows.clear();
                body
            };
            self.render_transcript(frame, transcript);
            self.render_attachment(frame, chip);
            frame.render_widget(&self.composer, input);
            self.render_composer_meta(frame, input);
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
        let width = usize::from(inner.width);
        let mut top = inner.y.saturating_add(1);
        let mut hidden = false;
        for worker in &self.workers {
            let (glyph, colour, state) = worker_state(worker);
            let preview = self.preview_of(worker.id).and_then(last_line);
            let height = if preview.is_some() { 5_u16 } else { 4 };
            let used = top.saturating_sub(inner.y);
            if used.saturating_add(height) > inner.height {
                hidden = true;
                break;
            }
            let unread = self.unread_of(worker.id);
            let indent = "  ".repeat(worker.depth);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!("{indent}{glyph} "), Style::new().fg(colour)),
                    Span::styled(
                        format!("a{}", worker.id),
                        Style::new().fg(if self.selected == Some(worker.id) { theme::text() } else { theme::info() }),
                    ),
                    Span::styled(format!(" {state}"), Style::new().fg(theme::muted())),
                    Span::styled(
                        if unread == 0 { String::new() } else { format!(" · {unread} new") },
                        Style::new().fg(theme::warning()),
                    ),
                ]),
                Line::from(Span::styled(short_dir(&worker.dir), Style::new().fg(theme::faint()))),
            ];
            let task = worker.task.lines().next().unwrap_or_default().trim();
            lines.push(Line::from(Span::styled(truncate(task, width), Style::new().fg(theme::muted()))));
            if let Some(preview) = preview {
                lines.push(Line::from(Span::styled(
                    format!("▸ {}", truncate(preview, width.saturating_sub(2))),
                    Style::new().fg(theme::faint()),
                )));
            }
            lines.push(Line::default());
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
            spans.push(Span::styled(format!("{glyph} thinking "), Style::new().fg(theme::accent())));
            let secs = self.busy_since.elapsed().as_secs_f32();
            spans.push(Span::styled(format!("{secs:.0}s"), Style::new().fg(theme::muted())));
        } else if self.draining {
            spans.push(Span::styled("draining, quits when workers idle", Style::new().fg(theme::warning())));
        } else {
            spans.push(Span::styled("● idle", Style::new().fg(theme::muted())));
        }
        let working = self.workers.iter().filter(|w| w.pending > 0 && !w.finished).count();
        let paused = self.workers.iter().filter(|w| w.paused).count();
        let idle = self.workers.len().saturating_sub(working + paused);
        let paused = if paused == 0 { String::new() } else { format!("{paused} paused · ") };
        spans
            .push(Span::styled(format!(" · {working} working · {paused}{idle} idle"), Style::new().fg(theme::muted())));
        if let Some(scroll) = self.scrolled() {
            spans.push(Span::styled(format!(" · ↑{scroll} lines"), Style::new().fg(theme::warning())));
        }
        let used = Line::from(spans.clone()).width();
        let hints = if self.in_worker_view() { WORKER_HINTS.as_slice() } else { HINTS.as_slice() };
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

    /// Whether a worker view is open: the picked worker is still in the roster and the view is not closed.
    pub(super) fn in_worker_view(&self) -> bool {
        self.viewed().is_some()
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

    /// One worker's view: a breadcrumb, its state, its task, then its own lines of the transcript.
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
        let crumb = Line::from(vec![
            Span::styled("agents ", Style::new().fg(theme::muted())),
            Span::styled("› ", Style::new().fg(theme::faint())),
            Span::styled(format!("agent {}", worker.id), Style::new().fg(theme::info())),
        ]);
        let mut line = vec![
            Span::styled(format!("{glyph} "), Style::new().fg(colour)),
            Span::styled(state, Style::new().fg(theme::muted())),
            Span::styled(format!(" · {}", worker.dir.display()), Style::new().fg(theme::faint())),
        ];
        let hint = "esc back to agents";
        let used = Line::from(line.clone()).width();
        line.push(Span::raw(" ".repeat(width.saturating_sub(used + text_width(hint)).max(1))));
        line.push(Span::styled(hint, Style::new().fg(theme::faint())));
        let mut head = vec![crumb, Line::from(line), Line::default()];
        for row in wrap(&worker.task, width) {
            head.push(Line::from(Span::styled(row, Style::new().fg(theme::text()))));
        }
        if let Some(preview) = self.preview_of(worker.id).map(str::trim).filter(|text| !text.is_empty()) {
            head.push(Line::default());
            for row in wrap(preview, width.saturating_sub(2)) {
                head.push(Line::from(vec![
                    Span::styled("▸ ", Style::new().fg(theme::info())),
                    Span::styled(row, Style::new().fg(theme::muted())),
                ]));
            }
        }
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
            // Esc leaves the view; it never quits the app.
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
    }

    pub(super) fn toggle_workers(&mut self) {
        self.worker_panel = !self.worker_panel;
        self.dirty = true;
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
            self.recall = None;
            self.dirty = true;
        }
    }

    /// Replaces the composer's text with a recalled prompt, caret at its end.
    fn refill(&mut self, text: &str) {
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let cursor = (lines.len().saturating_sub(1), lines.last().map_or(0, |line| line.chars().count()));
        self.composer.set_lines(lines, cursor);
        self.dirty = true;
    }
}

fn worker_state(worker: &AgentView) -> (&'static str, Color, String) {
    if worker.stopped {
        return ("■", theme::faint(), "stopped".into());
    }
    if worker.paused {
        return ("❚❚", theme::info(), "paused".into());
    }
    if worker.pending > 0 && !worker.finished {
        let secs = worker.busy_for.unwrap_or_default().as_secs();
        let read_only = if worker.read_only { " · read-only" } else { "" };
        return ("●", theme::warning(), format!("working {secs}s{read_only}"));
    }
    if worker.failed || worker.pending > 0 {
        return ("✕", theme::error(), "failed".into());
    }
    let read_only = if worker.read_only { " · read-only" } else { "" };
    ("○", theme::faint(), format!("idle{read_only}"))
}

/// The last non-empty line of a streamed preview.
fn last_line(text: &str) -> Option<&str> {
    text.lines().rev().find(|line| !line.trim().is_empty())
}

/// The last component of the worker's directory, so a narrow panel stays readable.
fn short_dir(dir: &Path) -> String {
    dir.file_name().map_or_else(|| dir.display().to_string(), |name| name.to_string_lossy().into_owned())
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
    use std::path::PathBuf;

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
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
    fn a_collapsed_tool_card_keeps_two_rows_and_counts_the_rest() {
        let mut app = App::new("m");
        let text = format!("first line\n{}tail", "word ".repeat(40));
        tool_call(&mut app, 0, "spawn_agent", &format!("/tmp · {text}"));
        let whole = app.lead.entries[0].lines(40, 0, true);
        let hidden = whole.len() - 2;
        let collapsed = app.lead.entries[0].lines(40, 0, false);
        assert_eq!(collapsed.len(), 3, "two body rows and the row that counts the rest: {collapsed:?}");
        let joined = row_text(&collapsed);
        assert!(joined.contains("first line"), "{joined}");
        assert!(!joined.contains("tail"), "the end of the task waits for ctrl+o: {joined}");
        assert!(joined.contains(&format!("… {hidden} more lines")), "{joined}");
        assert_eq!(collapsed[2].spans[0].style.fg, Some(theme::faint()), "the count is faint");
        let rendered = screen(&mut app, 100, 24);
        assert!(!rendered.contains("tail"), "the rendered card stops at its first rows: {rendered}");
        assert!(rendered.contains("more lines"), "the launch render counts what it hides: {rendered}");
    }

    #[test]
    fn a_card_body_that_fits_two_rows_gets_no_hint_row() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, "spawn_agent", "/tmp · one\ntwo");
        let lines = app.lead.entries[0].lines(80, 0, false);
        assert_eq!(lines.len(), 2, "both rows fit, so nothing is hidden: {lines:?}");
        assert!(!row_text(&lines).contains("more"), "no count without something hidden");
    }

    #[test]
    fn a_card_that_hides_one_row_says_line_and_not_lines() {
        let mut app = App::new("m");
        tool_call(&mut app, 0, "spawn_agent", "/tmp · one\ntwo\nthree");
        let lines = app.lead.entries[0].lines(80, 0, false);
        let joined = row_text(&lines);
        assert_eq!(lines.len(), 3, "two rows and the count: {lines:?}");
        assert!(joined.contains("… 1 more line"), "{joined}");
        assert!(!joined.contains("more lines"), "one hidden row is a line: {joined}");
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
        assert_eq!(worker_entries(&app, 2)[0].lines(80, 0, false).len(), 1, "F3 leaves one row per call");
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
        dead.pending = 1;
        dead.finished = true;
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
        app.apply(UiEvent::TurnBoundary);
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
        app.apply(UiEvent::TurnBoundary);
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
        app.apply(UiEvent::TurnBoundary);
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
        child.finished = true;
        child.dir = PathBuf::from("/tmp/overseer");
        app.apply(UiEvent::Agents(vec![child]));
        app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Finished, text: "tests pass".into() });
        let screen = screen(&mut app, 100, 24);
        assert!(screen.contains("spawned two agents"), "{screen}");
        assert!(screen.contains("agent 2 finished: tests pass"), "{screen}");
        assert!(screen.contains("overseer"), "{screen}");
        assert!(screen.contains("idle"), "{screen}");
        assert!(screen.contains("┃"), "the composer keeps its bar: {screen}");
        assert!(screen.contains("Ask agent 1"), "{screen}");
        assert!(screen.contains("test-model"), "the status line names the model: {screen}");
        assert!(screen.contains(version::VERSION), "{screen}");
    }

    #[test]
    fn agent_preview_shows_the_live_line_then_clears() {
        let mut app = App::new("m");
        let mut busy = agent_view(2, "long job");
        busy.pending = 1;
        busy.busy_for = Some(Duration::from_secs(3));
        app.apply(UiEvent::Agents(vec![busy]));
        app.apply(UiEvent::AgentDelta { id: 2, text: "reading the config".into() });
        let rendered = screen(&mut app, 100, 24);
        assert!(rendered.contains("reading the config"), "{rendered}");
        app.apply(UiEvent::Agent { id: 2, kind: AgentEvent::Finished, text: "done".into() });
        let rendered = screen(&mut app, 100, 24);
        assert!(!rendered.contains("reading the config"), "{rendered}");
    }

    #[test]
    fn a_dead_agent_shows_as_failed() {
        let mut app = App::new("m");
        let mut dead = agent_view(2, "long job");
        dead.pending = 1;
        dead.finished = true;
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
            parent: Some(Swarm::ROOT),
            depth: 1,
            dir: PathBuf::from("/tmp/repo"),
            task: task.into(),
            pending: 0,
            failed: false,
            finished: false,
            stopped: false,
            paused: false,
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
        app.apply(UiEvent::Agent { id: 3, kind: AgentEvent::Finished, text: "done".into() });
        click_first_agent(&mut app);
        assert!(app.in_worker_view());
        let view = screen(&mut app, 100, 24);
        assert!(view.contains("agents › agent 3"), "{view}");
        assert!(view.contains("fix the nav"), "the view shows the task handed to the agent: {view}");
        assert!(view.contains("esc back to agents"), "the way out is on the header: {view}");
        assert!(view.contains("[agent 3] done done"), "the view shows the agent's own lines: {view}");
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
    fn a_tool_call_hangs_off_its_message_and_the_next_entry_starts_a_paragraph() {
        let mut app = App::new("m");
        app.apply(UiEvent::Answer("on it".into()));
        tool_call(&mut app, 0, "bash", "ls");
        screen(&mut app, 60, 16);
        let rows: Vec<String> = app.lead.transcript.iter().map(|line| row_text(std::slice::from_ref(line))).collect();
        assert_eq!(rows.len(), 2, "the call sits under its message, no blank row between: {rows:?}");
        assert!(rows[0].contains("on it"), "{rows:?}");
        assert!(rows[1].contains("$ ls"), "{rows:?}");
        app.apply(UiEvent::Notice("agent 2 is idle".into()));
        screen(&mut app, 60, 16);
        let rows: Vec<String> = app.lead.transcript.iter().map(|line| row_text(std::slice::from_ref(line))).collect();
        assert_eq!(rows.len(), 4, "a notice opens a new paragraph: {rows:?}");
        assert!(rows[2].is_empty(), "{rows:?}");
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
}
