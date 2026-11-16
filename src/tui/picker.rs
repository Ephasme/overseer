//! The `-r` session picker: a minimal alternate-screen list. Arrow keys move, Enter picks, Esc or
//! `q` cancels. No mouse. The row text is unit-tested; the key loop needs a real terminal.

use crate::config;
use crate::session_store;
use anyhow::Result;
use ratatui::crossterm::event::{self, Event as Input, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the loop waits for a key.
const POLL: Duration = Duration::from_millis(50);

/// Runs the picker. Returns the chosen session id, or `None` when the user cancelled
/// (Esc, `q`) or there is nothing to show.
pub(crate) fn run(home: &Path) -> Result<Option<String>> {
    let infos = session_store::sessions(&config::state_dir(home));
    if infos.is_empty() {
        println!("no previous session");
        return Ok(None);
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let rows: Vec<String> = infos.iter().map(|info| session_store::picker_row(info, now)).collect();
    let mut state = ListState::default();
    state.select(Some(0));
    let mut terminal = ratatui::try_init()?;
    let picked = loop {
        let title = Line::from("resume which session? (up/down move, Enter picks, Esc cancels)").centered();
        let items: Vec<ListItem> =
            picker_rows(&rows, state.selected().unwrap_or(0)).into_iter().map(ListItem::new).collect();
        let list = List::new(items);
        terminal.draw(|frame| {
            let [_, header, body, _] = Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(1),
                Constraint::Length(u16::try_from(rows.len()).unwrap_or(u16::MAX)),
                Constraint::Fill(1),
            ])
            .areas(frame.area());
            frame.render_widget(title.clone(), header);
            frame.render_stateful_widget(list, body, &mut state);
        })?;
        if !event::poll(POLL)? {
            continue;
        }
        let Input::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => break None,
            KeyCode::Down | KeyCode::Char('j') => {
                let end = rows.len().saturating_sub(1);
                let i = state.selected().map_or(0, |i| i.saturating_add(1)).min(end);
                state.select(Some(i));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = state.selected().unwrap_or(0).saturating_sub(1);
                state.select(Some(i));
            }
            KeyCode::Enter => {
                break state.selected().and_then(|i| infos.get(i)).map(|info| info.id.clone());
            }
            _ => {}
        }
    };
    ratatui::restore();
    Ok(picked)
}

/// The rendered rows for `row_texts`, with the row at `selected` bolded.
fn picker_rows(row_texts: &[String], selected: usize) -> Vec<Line<'static>> {
    row_texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            if i == selected {
                Line::from(text.clone()).style(Style::default().add_modifier(Modifier::BOLD))
            } else {
                Line::from(text.clone())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_highlight_the_selected_one_only() {
        let rows = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let lines = picker_rows(&rows, 1);
        for (i, line) in lines.iter().enumerate() {
            let bold = line.style.add_modifier.contains(Modifier::BOLD);
            assert_eq!(bold, i == 1, "row {i}");
        }
    }
}
