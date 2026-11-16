//! The `-r` session picker: a minimal alternate-screen list. Arrow keys move, Enter picks, Esc, `q`
//! or ctrl+c cancels. No mouse. The row text is unit-tested; the key loop needs a real terminal.

use crate::session_store;
use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the loop waits for a key before looking for a resize.
const POLL: Duration = Duration::from_millis(50);

/// Runs the picker over the sessions under `state`. Returns the chosen session id, or `None` when
/// the user cancelled or there is nothing to show.
pub(crate) fn run(state: &Path) -> Result<Option<String>> {
    let infos = session_store::sessions(state);
    if infos.is_empty() {
        println!("no previous session");
        return Ok(None);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let rows: Vec<ListItem> = infos.iter().map(|info| ListItem::new(session_store::picker_row(info, now))).collect();
    let ids: Vec<String> = infos.into_iter().map(|info| info.id).collect();
    // The rows are static: the highlight is a list style, so nothing is rebuilt per frame.
    let list = List::new(rows).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut terminal = ratatui::try_init()?;
    // Every `?` below goes through `restore`, or an I/O error leaves the terminal in raw mode and
    // on the alternate screen.
    let picked = pick(&mut terminal, &list, &ids);
    ratatui::restore();
    picked
}

/// The key loop. Drawn once, then only after the selection or the terminal size changes: a picker
/// over a static list has no reason to redraw on every poll timeout.
fn pick(terminal: &mut DefaultTerminal, list: &List<'_>, ids: &[String]) -> Result<Option<String>> {
    let title = Line::from("resume which session? (up/down move, Enter picks, Esc cancels)").centered();
    let mut selection = ListState::default();
    selection.select(Some(0));
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| {
                let [_, header, body, _] = Layout::vertical([
                    Constraint::Fill(1),
                    Constraint::Length(1),
                    Constraint::Length(u16::try_from(ids.len()).unwrap_or(u16::MAX)),
                    Constraint::Fill(1),
                ])
                .areas(frame.area());
                frame.render_widget(title.clone(), header);
                frame.render_stateful_widget(list, body, &mut selection);
            })?;
            dirty = false;
        }
        if !event::poll(POLL)? {
            continue;
        }
        match event::read()? {
            Input::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                // Raw mode turns off ISIG, so ctrl+c arrives as a key: `q` must not be the only exit.
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
                KeyCode::Down | KeyCode::Char('j') => {
                    let end = ids.len().saturating_sub(1);
                    let next = selection.selected().map_or(0, |i| i.saturating_add(1)).min(end);
                    selection.select(Some(next));
                    dirty = true;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selection.select(Some(selection.selected().unwrap_or(0).saturating_sub(1)));
                    dirty = true;
                }
                KeyCode::Enter => {
                    return Ok(selection.selected().and_then(|i| ids.get(i)).cloned());
                }
                _ => {}
            },
            Input::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
}
