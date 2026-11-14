mod app;

use crate::event::UiEvent;
use crate::fleet::Event;
use anyhow::Result;
use app::App;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event as Input,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use std::io::stdout;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

/// How long the loop waits for a key before draining the core's events.
const POLL: Duration = Duration::from_millis(50);

/// Runs the full-screen frontend until the user quits or the core ends.
pub(crate) fn run(ui: Receiver<UiEvent>, input: Sender<Event>, model: &str) -> Result<()> {
    let mut app = App::new(model);
    let mut terminal = ratatui::try_init()?;
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    let result = event_loop(&mut terminal, &mut app, ui, input);
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

/// The channels are owned here: the loop ends the frontend when either side disconnects.
#[allow(clippy::needless_pass_by_value)]
fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    ui: Receiver<UiEvent>,
    input: Sender<Event>,
) -> Result<()> {
    loop {
        if app.take_dirty() {
            terminal.draw(|frame| app.render(frame))?;
        }
        if event::poll(POLL)? {
            match event::read()? {
                Input::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, &input, key),
                Input::Paste(text) => app.insert(&text),
                Input::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.wheel(true),
                    MouseEventKind::ScrollDown => app.wheel(false),
                    MouseEventKind::Down(MouseButton::Left) => app.click(mouse.column, mouse.row),
                    _ => {}
                },
                Input::Resize(..) => app.resize(),
                _ => {}
            }
        }
        loop {
            match ui.try_recv() {
                Ok(event) => app.apply(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.lost_core();
                    break;
                }
            }
        }
        app.tick();
        if app.quit {
            // The events that ended the session are already applied: show them once before leaving.
            if app.take_dirty() {
                terminal.draw(|frame| app.render(frame))?;
            }
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, input: &Sender<Event>, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('c') if ctrl => quit(app, input),
        KeyCode::Char('d') if ctrl => {
            let _ = input.send(Event::Eof);
            app.drain();
        }
        KeyCode::Char('j') if ctrl => app.insert("\n"),
        KeyCode::Char('v') if ctrl => app.paste(),
        KeyCode::Char('p') if ctrl => app.recall_previous(),
        KeyCode::Char('n') if ctrl => app.recall_next(),
        KeyCode::Enter if !alt && !shift => submit(app, input),
        KeyCode::PageUp => app.scroll_up(),
        KeyCode::PageDown => app.scroll_down(),
        KeyCode::Home => app.scroll_top(),
        KeyCode::End => app.scroll_bottom(),
        KeyCode::F(2) => app.toggle_workers(),
        KeyCode::F(3) => app.toggle_tool_output(),
        _ => app.input(key),
    }
}

fn quit(app: &mut App, input: &Sender<Event>) {
    let _ = input.send(Event::Exit);
    app.quit = true;
}

fn submit(app: &mut App, input: &Sender<Event>) {
    let prompt = app.take_prompt();
    if prompt.is_empty() {
        return;
    }
    app.echo(&prompt);
    match Event::from_line(&prompt) {
        Some(Event::Exit) => quit(app, input),
        Some(event) => {
            let _ = input.send(event);
        }
        None => {}
    }
}
