use crate::event::UiEvent;
use crate::fleet::{Progress, WorkerEvent, WorkerView};
use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};
use std::path::Path;
use std::time::{Duration, Instant};
use tui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthChar;

/// Frames of the busy spinner.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Rows of a tool result the transcript shows before counting the rest.
const TOOL_LINES: usize = 6;

/// Rows the composer grows to before it scrolls.
const COMPOSER_ROWS: u16 = 8;

/// Width of the worker panel, borders included.
const WORKERS_WIDTH: u16 = 38;

/// Rows one wheel notch scrolls.
const WHEEL_LINES: u16 = 3;

/// Redraw interval of the spinner.
const TICK: Duration = Duration::from_millis(120);

/// Longest worker preview kept, in characters.
const PREVIEW_CHARS: usize = 400;

/// Key hints on the right of the status line.
const HINT: &str = "enter send · ctrl+j newline · F2 workers · ctrl+d quit";

const DIM: Style = Style::new().fg(Color::DarkGray);
const CYAN: Color = Color::Cyan;
const MAGENTA: Color = Color::Magenta;
const GREEN: Color = Color::Green;
const RED: Color = Color::Red;
const BLUE: Color = Color::Blue;
const YELLOW: Color = Color::Yellow;

/// What a transcript line is about; decides its gutter and colours.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    You,
    Overseer,
    Tool,
    Worker(WorkerEvent),
    Progress,
    Notice,
    Error,
    Debug,
}

struct Entry {
    role: Role,
    /// Tool name, or the worker label.
    title: String,
    /// What the entry says: a message, a tool summary, a worker verb.
    body: String,
    /// Tool result, once the call returns.
    output: Option<String>,
    secs: Option<f64>,
}

impl Entry {
    fn new(role: Role, title: String, body: String) -> Self {
        Self { role, title, body, output: None, secs: None }
    }

    fn finish(&mut self, output: String, secs: f64) {
        self.output = Some(output);
        self.secs = Some(secs);
    }

    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width).max(8);
        match self.role {
            Role::You => block("› ", Style::new().fg(CYAN), &self.body, Style::new(), width),
            Role::Overseer => block("● ", Style::new().fg(MAGENTA), &self.body, Style::new(), width),
            Role::Tool => self.tool_lines(width),
            Role::Worker(kind) => {
                let (label, colour) = match kind {
                    WorkerEvent::Reply => ("says", CYAN),
                    WorkerEvent::Finished => ("done", GREEN),
                    WorkerEvent::Failed => ("failed", RED),
                };
                let prefix = format!("[{}] {label} ", self.title);
                block(&prefix, Style::new().fg(colour), &self.body, Style::new(), width)
            }
            Role::Progress => {
                vec![Line::from(Span::styled(truncate(&format!("· {} {}", self.title, self.body), width), DIM))]
            }
            Role::Notice => block("· ", Style::new().fg(BLUE), &self.body, DIM, width),
            Role::Error => block("error ", Style::new().fg(RED), &self.body, Style::new().fg(RED), width),
            Role::Debug => block("… ", DIM, &self.body, DIM, width),
        }
    }

    fn tool_lines(&self, width: usize) -> Vec<Line<'static>> {
        let head = if self.body.is_empty() {
            format!("⚙ {}", self.title)
        } else {
            format!("⚙ {} {}", self.title, self.body)
        };
        let mut lines = vec![Line::from(Span::styled(truncate(&head, width), Style::new().fg(YELLOW)))];
        let Some(output) = &self.output else {
            lines.push(Line::from(Span::styled("  … running", DIM)));
            return lines;
        };
        let failed = output.starts_with("error:");
        let style = if failed { Style::new().fg(RED) } else { DIM };
        let mut rows: Vec<String> = Vec::new();
        for source in output.trim_end().lines() {
            rows.extend(wrap(source, width.saturating_sub(2)));
        }
        if rows.is_empty() {
            rows.push("(no output)".into());
        }
        let total = rows.len();
        for row in rows.iter().take(TOOL_LINES) {
            lines.push(Line::from(vec![Span::raw("  "), Span::styled(row.clone(), style)]));
        }
        if total > TOOL_LINES {
            lines.push(Line::from(Span::styled(format!("  … {} more lines", total - TOOL_LINES), DIM)));
        }
        if let Some(secs) = self.secs
            && let Some(head) = lines.first_mut()
        {
            head.spans.push(Span::styled(format!("  {secs:.1}s"), DIM));
        }
        lines
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

/// Word-wraps one line of text to `width` columns, cutting words wider than the line.
///
/// Leading whitespace stays on the first row, so indented tool output keeps its shape.
fn wrap(source: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![source.to_string()];
    }
    let trimmed = source.trim_start();
    let lead: String = source.chars().take(source.len() - trimmed.len()).collect();
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = text_width(&lead);
    for word in trimmed.split_whitespace() {
        let mut rest = word;
        loop {
            let space = usize::from(!row.is_empty());
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
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
                continue;
            }
            let (head, tail) = split_at_width(rest, width.saturating_sub(used).max(1));
            rows.push(format!("{lead}{head}"));
            rest = tail;
            used = 0;
            if rest.is_empty() {
                break;
            }
        }
    }
    rows.push(format!("{lead}{row}"));
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
    if text_width(text) <= width {
        return text.to_string();
    }
    let (head, _) = split_at_width(text, width.saturating_sub(1));
    format!("{head}…")
}

fn composer(model: &str) -> TextArea<'static> {
    let mut area = TextArea::default();
    area.set_block(Block::bordered().border_style(DIM).title(format!(" message · {model} ")).title_style(DIM));
    area.set_placeholder_text("Ask the overseer, /resume <id>, or exit");
    area.set_wrap_mode(WrapMode::WordOrGlyph);
    area.set_max_rows(COMPOSER_ROWS);
    area
}

#[allow(clippy::struct_excessive_bools)]
pub(super) struct App {
    entries: Vec<Entry>,
    /// The transcript flattened at `transcript_width`; `transcript_dirty` asks for a rebuild.
    transcript: Vec<Line<'static>>,
    transcript_width: u16,
    transcript_dirty: bool,
    composer: TextArea<'static>,
    model: String,
    history: Vec<String>,
    /// Index into `history` while recalling with ctrl+p and ctrl+n.
    recall: Option<usize>,
    /// Where each tool call's entry landed, by the id the core sends.
    tools: Vec<Option<usize>>,
    /// Index of the entry the overseer's streamed text is filling.
    live: Option<usize>,
    workers: Vec<WorkerView>,
    /// Last streamed text per worker, for the panel.
    previews: Vec<(usize, String)>,
    /// Follow new lines; off while the user reads older ones.
    follow: bool,
    /// Lines scrolled up from the bottom while `follow` is off.
    scroll: u16,
    /// Height and total rows of the last transcript frame.
    view: (u16, u16),
    worker_panel: bool,
    busy: bool,
    busy_since: Instant,
    last_tick: Instant,
    spinning: usize,
    draining: bool,
    dirty: bool,
    pub(super) quit: bool,
}

impl App {
    pub(super) fn new(model: &str) -> Self {
        Self {
            entries: Vec::new(),
            transcript: Vec::new(),
            transcript_width: 0,
            transcript_dirty: true,
            composer: composer(model),
            model: model.to_string(),
            history: Vec::new(),
            recall: None,
            tools: Vec::new(),
            live: None,
            workers: Vec::new(),
            previews: Vec::new(),
            follow: true,
            scroll: 0,
            view: (0, 0),
            worker_panel: true,
            busy: false,
            busy_since: Instant::now(),
            last_tick: Instant::now(),
            spinning: 0,
            draining: false,
            dirty: true,
            quit: false,
        }
    }

    pub(super) fn apply(&mut self, event: UiEvent) {
        self.dirty = true;
        self.transcript_dirty = true;
        match event {
            UiEvent::Notice(text) => self.push(Role::Notice, String::new(), text),
            UiEvent::Error(text) => self.push(Role::Error, String::new(), text),
            UiEvent::Debug(text) => self.push(Role::Debug, String::new(), text),
            UiEvent::Delta(text) => self.stream(&text),
            UiEvent::Answer(text) => self.answer(text),
            UiEvent::ToolStarted { id, tool, summary } => {
                let index = self.entries.len();
                self.entries.push(Entry::new(Role::Tool, tool, summary));
                if self.tools.len() <= id {
                    self.tools.resize(id + 1, None);
                }
                if let Some(slot) = self.tools.get_mut(id) {
                    *slot = Some(index);
                }
            }
            UiEvent::ToolFinished { id, output, secs } => {
                let index = self.tools.get(id).copied().flatten();
                if let Some(entry) = index.and_then(|i| self.entries.get_mut(i)) {
                    entry.finish(output, secs);
                }
            }
            UiEvent::Worker { id, kind, text } => {
                self.clear_preview(id);
                self.push(Role::Worker(kind), format!("worker {id}"), text);
            }
            UiEvent::WorkerDelta { id, text } => self.preview(id, &text),
            UiEvent::Progress { id, progress } => {
                if let Progress::Finished { tool, secs } = progress {
                    self.push(Role::Progress, format!("worker {id}"), format!("{tool} · {secs:.1}s"));
                }
            }
            UiEvent::Workers(workers) => self.workers = workers,
            UiEvent::Busy(moving) => {
                self.busy = moving;
                if moving {
                    self.busy_since = Instant::now();
                } else {
                    // A failed turn never sends an answer: its streamed text stays as the record.
                    self.live = None;
                }
            }
            UiEvent::Done => self.quit = true,
        }
    }

    /// Appends streamed overseer text, starting an entry on the first delta.
    ///
    /// The core sends `"\n"` between model calls of one turn, which becomes a blank line.
    fn stream(&mut self, text: &str) {
        if text == "\n" {
            if let Some(entry) = self.live.and_then(|index| self.entries.get_mut(index))
                && !entry.body.is_empty()
                && !entry.body.ends_with("\n\n")
            {
                entry.body.push_str("\n\n");
            }
            return;
        }
        if self.live.is_none() {
            let index = self.entries.len();
            self.push(Role::Overseer, String::new(), String::new());
            self.live = Some(index);
        }
        if let Some(entry) = self.live.and_then(|index| self.entries.get_mut(index)) {
            entry.body.push_str(text);
        }
    }

    /// The turn's final text: it replaces what streamed when a backend sends nothing.
    fn answer(&mut self, text: String) {
        match self.live.take().and_then(|index| self.entries.get_mut(index)) {
            Some(entry) if entry.body.trim().is_empty() => entry.body = text,
            Some(entry) if !entry.body.trim_end().ends_with(text.trim()) && !text.trim().is_empty() => {
                entry.body.push_str("\n\n");
                entry.body.push_str(&text);
            }

            Some(_) => {}
            None => self.push(Role::Overseer, String::new(), text),
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

    fn push(&mut self, role: Role, title: String, body: String) {
        self.entries.push(Entry::new(role, title, body));
    }

    /// Whether a frame is due; clears the flag.
    pub(super) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// A resize changes the transcript width, so its cache is rebuilt on the next draw.
    pub(super) fn resize(&mut self) {
        self.transcript_dirty = true;
        self.dirty = true;
    }

    /// Advances the spinner; says whether a redraw is needed.
    pub(super) fn tick(&mut self) -> bool {
        if !self.busy || self.last_tick.elapsed() < TICK {
            return false;
        }
        self.last_tick = Instant::now();
        self.spinning = self.spinning.wrapping_add(1);
        self.dirty = true;
        true
    }

    pub(super) fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let rows = self.composer.measure(area.width).preferred_rows;
        let rows = rows.clamp(3, area.height.saturating_sub(4).max(3));
        let [body, input, status] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(rows), Constraint::Length(1)]).areas(area);
        let transcript = if self.worker_panel && body.width > WORKERS_WIDTH + 40 {
            let [left, right] =
                Layout::horizontal([Constraint::Min(40), Constraint::Length(WORKERS_WIDTH)]).areas(body);
            self.render_workers(frame, right);
            left
        } else {
            body
        };
        self.render_transcript(frame, transcript);
        frame.render_widget(&self.composer, input);
        self.render_status(frame, status);
    }

    fn render_transcript(&mut self, frame: &mut Frame, area: Rect) {
        let frame_block = Block::new().padding(Padding::horizontal(1));
        let inner = frame_block.inner(area);
        frame.render_widget(frame_block, area);
        let width = inner.width.max(1);
        if self.transcript_dirty || self.transcript_width != width {
            self.transcript = flatten(&self.entries, width);
            self.transcript_width = width;
            self.transcript_dirty = false;
        }
        let total = u16::try_from(self.transcript.len()).unwrap_or(u16::MAX);
        let bottom = total.saturating_sub(inner.height);
        let offset = if self.follow { bottom } else { bottom.saturating_sub(self.scroll.min(bottom)) };
        self.view = (inner.height, total);
        let paragraph = Paragraph::new(self.transcript.as_slice()).scroll((offset, 0));
        frame.render_widget(paragraph, inner);
    }

    fn render_workers(&self, frame: &mut Frame, area: Rect) {
        let panel = Block::bordered().border_style(DIM).title(" workers ");
        let inner = panel.inner(area);
        frame.render_widget(panel, area);
        if self.workers.is_empty() {
            frame.render_widget(Paragraph::new(Span::styled("none yet", DIM)), inner);
            return;
        }
        let width = usize::from(inner.width.saturating_sub(2));
        let room = usize::from(inner.height);
        let mut rows: Vec<Line<'static>> = Vec::new();
        for worker in &self.workers {
            let (glyph, colour, state) = worker_state(worker);
            let preview = self.preview_of(worker.id).and_then(last_line);
            let height = if preview.is_some() { 5 } else { 4 };
            if rows.len() + height > room {
                break;
            }
            rows.push(Line::from(vec![
                Span::styled(format!("{} {glyph} ", worker.id), Style::new().fg(colour)),
                Span::styled(state, Style::new().fg(colour)),
            ]));
            rows.push(Line::from(Span::styled(format!("  {}", short_dir(&worker.dir)), DIM)));
            let task = worker.task.lines().next().unwrap_or_default();
            rows.push(Line::from(Span::styled(format!("  {}", truncate(task, width)), DIM)));
            if let Some(preview) = preview {
                rows.push(Line::from(Span::styled(
                    format!("  ▸ {}", truncate(preview, width.saturating_sub(2))),
                    Style::new().fg(Color::Gray),
                )));
            }
            rows.push(Line::default());
        }
        if rows.len() < self.workers.len() * 4 {
            rows.push(Line::from(Span::styled("…", DIM)));
        }
        frame.render_widget(Paragraph::new(rows), inner);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        if self.busy {
            let glyph = SPINNER.get(self.spinning % SPINNER.len()).copied().unwrap_or("⠋");
            spans.push(Span::styled(format!("{glyph} thinking "), Style::new().fg(YELLOW)));
            spans.push(Span::styled(format!("{:.0}s", self.busy_since.elapsed().as_secs_f32()), DIM));
        } else if self.draining {
            spans.push(Span::styled("draining, quits when workers idle", Style::new().fg(YELLOW)));
        } else {
            spans.push(Span::styled("● idle", DIM));
        }
        let working = self.workers.iter().filter(|w| w.pending > 0).count();
        let idle = self.workers.len().saturating_sub(working);
        spans.push(Span::styled(format!(" │ {working} working · {idle} idle"), DIM));
        if !self.follow {
            spans.push(Span::styled(format!(" │ ↑{} lines", self.scroll), Style::new().fg(YELLOW)));
        }
        let used = Line::from(spans.clone()).width();
        let room = usize::from(area.width).saturating_sub(used + HINT.len() + 1);
        spans.push(Span::raw(" ".repeat(room)));
        spans.push(Span::styled(HINT, DIM));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The prompt as submitted, trimmed; the composer keeps its settings but starts empty.
    pub(super) fn take_prompt(&mut self) -> String {
        let text = self.composer.lines().join("\n").trim().to_string();
        self.composer = composer(&self.model);
        self.recall = None;
        if !text.is_empty() {
            self.history.push(text.clone());
        }
        self.dirty = true;
        text
    }

    pub(super) fn echo(&mut self, prompt: &str) {
        self.push(Role::You, String::new(), prompt.to_string());
        self.transcript_dirty = true;
        self.dirty = true;
    }

    pub(super) fn insert(&mut self, text: &str) {
        self.composer.insert_str(text);
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

    pub(super) fn drain(&mut self) {
        self.draining = true;
        self.dirty = true;
    }

    /// The core thread vanished without saying `Done`: show it and quit rather than wait forever.
    pub(super) fn lost_core(&mut self) {
        self.push(Role::Error, String::new(), "overseer stopped unexpectedly".into());
        self.transcript_dirty = true;
        self.dirty = true;
        self.quit = true;
    }

    fn page(&self) -> u16 {
        self.view.0.saturating_sub(2).max(1)
    }

    pub(super) fn scroll_up(&mut self) {
        self.follow = false;
        let bottom = self.view.1.saturating_sub(self.view.0);
        self.scroll = self.scroll.saturating_add(self.page()).min(bottom);
        self.dirty = true;
    }

    pub(super) fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_sub(self.page());
        if self.scroll == 0 {
            self.follow = true;
        }
        self.dirty = true;
    }

    pub(super) fn scroll_top(&mut self) {
        self.follow = false;
        self.scroll = self.view.1.saturating_sub(self.view.0);
        self.dirty = true;
    }

    pub(super) fn scroll_bottom(&mut self) {
        self.follow = true;
        self.scroll = 0;
        self.dirty = true;
    }

    pub(super) fn wheel(&mut self, up: bool) {
        let bottom = self.view.1.saturating_sub(self.view.0);
        let step = WHEEL_LINES.min(bottom);
        self.scroll = if up {
            self.follow = false;
            self.scroll.saturating_add(step).min(bottom)
        } else {
            self.scroll.saturating_sub(step)
        };
        if self.scroll == 0 {
            self.follow = true;
        }
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
            self.composer = composer(&self.model);
            self.recall = None;
            self.dirty = true;
        }
    }

    /// Replaces the composer's text with a recalled prompt, caret at its end.
    fn refill(&mut self, text: &str) {
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let cursor = (lines.len().saturating_sub(1), lines.last().map_or(0, String::len));
        self.composer.set_lines(lines, cursor);
        self.dirty = true;
    }
}

/// Every entry's rows, separated by a blank line.
fn flatten(entries: &[Entry], width: u16) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    for entry in entries {
        if !rows.is_empty() {
            rows.push(Line::default());
        }
        rows.extend(entry.lines(width));
    }
    rows
}

fn worker_state(worker: &WorkerView) -> (&'static str, Color, String) {
    if worker.pending > 0 {
        let secs = worker.busy_for.unwrap_or_default().as_secs();
        return ("●", YELLOW, format!("working {secs}s"));
    }
    if worker.failed {
        return ("✕", RED, "failed".into());
    }
    ("○", Color::DarkGray, "idle".into())
}

/// The last non-empty line of a streamed preview.
fn last_line(text: &str) -> Option<&str> {
    text.lines().rev().find(|line| !line.trim().is_empty())
}

/// The last component of the worker's directory, so a narrow panel stays readable.
fn short_dir(dir: &Path) -> String {
    dir.file_name().map_or_else(|| dir.display().to_string(), |name| name.to_string_lossy().into_owned())
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
    fn apply_pairs_tool_calls_with_their_results() {
        let mut app = App::new("m");
        app.apply(UiEvent::ToolStarted { id: 3, tool: "spawn_worker".into(), summary: "/tmp · do it".into() });
        let started = row_text(&app.entries[0].lines(80));
        assert!(started.contains("running"), "{started}");
        app.apply(UiEvent::ToolFinished { id: 3, output: "worker 1 started".into(), secs: 0.4 });
        let finished = row_text(&app.entries[0].lines(80));
        assert!(finished.contains("worker 1 started"), "{finished}");
        assert!(finished.contains("spawn_worker"), "{finished}");
        assert!(finished.contains("0.4s"), "{finished}");
    }

    #[test]
    fn streamed_text_is_one_entry_that_the_answer_completes() {
        let mut app = App::new("m");
        app.apply(UiEvent::Delta("Worker ".into()));
        app.apply(UiEvent::Delta("1 is done.".into()));
        assert_eq!(app.entries.len(), 1);
        app.apply(UiEvent::Answer("Worker 1 is done.".into()));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].body, "Worker 1 is done.");
    }

    #[test]
    fn model_call_boundary_becomes_a_blank_line() {
        let mut app = App::new("m");
        for delta in ["first call", "\n", "second call"] {
            app.apply(UiEvent::Delta(delta.into()));
        }
        assert_eq!(app.entries[0].body, "first call\n\nsecond call");
        assert_eq!(app.entries[0].lines(40).len(), 3);
    }

    #[test]
    fn a_boundary_before_any_text_opens_no_entry() {
        let mut app = App::new("m");
        app.apply(UiEvent::Delta("\n".into()));
        assert!(app.entries.is_empty());
    }

    #[test]
    fn a_new_turn_never_extends_the_previous_stream() {
        let mut app = App::new("m");
        app.apply(UiEvent::Delta("first turn".into()));
        app.apply(UiEvent::Busy(false));
        app.apply(UiEvent::Delta("second turn".into()));
        assert_eq!(app.entries.len(), 2);
        assert_eq!(app.entries[0].body, "first turn");
        assert_eq!(app.entries[1].body, "second turn");
    }

    #[test]
    fn answer_without_stream_still_appears() {
        let mut app = App::new("m");
        app.apply(UiEvent::Answer("plain reply".into()));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].body, "plain reply");
    }

    #[test]
    fn renders_transcript_worker_panel_and_status() {
        let mut app = App::new("test-model");
        app.apply(UiEvent::Notice("worker logs in /tmp/logs".into()));
        app.apply(UiEvent::Delta("spawned ".into()));
        app.apply(UiEvent::Delta("two workers".into()));
        app.apply(UiEvent::Worker { id: 1, kind: WorkerEvent::Finished, text: "tests pass".into() });
        app.workers = vec![WorkerView {
            id: 1,
            dir: PathBuf::from("/tmp/overseer"),
            task: "run the tests".into(),
            pending: 0,
            failed: false,
            finished: true,
            busy_for: None,
        }];
        let screen = screen(&mut app, 100, 24);
        assert!(screen.contains("spawned two workers"), "{screen}");
        assert!(screen.contains("[worker 1] done"), "{screen}");
        assert!(screen.contains("tests pass"), "{screen}");
        assert!(screen.contains("overseer"), "{screen}");
        assert!(screen.contains("idle"), "{screen}");
        assert!(screen.contains("message · test-model"), "{screen}");
    }

    #[test]
    fn worker_preview_shows_the_live_line_then_clears() {
        let mut app = App::new("m");
        app.workers = vec![WorkerView {
            id: 1,
            dir: PathBuf::from("/tmp/repo"),
            task: "long job".into(),
            pending: 1,
            failed: false,
            finished: false,
            busy_for: Some(Duration::from_secs(3)),
        }];
        app.apply(UiEvent::WorkerDelta { id: 1, text: "reading the config".into() });
        let rendered = screen(&mut app, 100, 24);
        assert!(rendered.contains("reading the config"), "{rendered}");
        app.apply(UiEvent::Worker { id: 1, kind: WorkerEvent::Finished, text: "done".into() });
        let rendered = screen(&mut app, 100, 24);
        assert!(!rendered.contains("reading the config"), "{rendered}");
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
}
