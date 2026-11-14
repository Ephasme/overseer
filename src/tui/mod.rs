mod app;
mod theme;

use crate::clipboard;
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

/// A clipboard read on its way back, with the id of the paste that asked for it.
type Copied = (u64, clipboard::Paste);

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
    // Clipboard reads come back here: they run on their own thread so a paste never stalls a frame.
    let (pasted, copied) = mpsc::channel::<Copied>();
    loop {
        if app.take_dirty() {
            terminal.draw(|frame| app.render(frame))?;
        }
        if event::poll(POLL)? {
            match event::read()? {
                Input::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, &input, &pasted, key),
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
        // A read that landed shows up in the frame after this one; a sender that went away with the
        // loop's reader is not news.
        while let Ok((id, found)) = copied.try_recv() {
            app.attach(id, found);
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

fn handle_key(app: &mut App, input: &Sender<Event>, pasted: &Sender<Copied>, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('c') if ctrl => quit(app, input),
        KeyCode::Char('d') if ctrl => {
            let _ = input.send(Event::Eof);
            app.drain();
        }
        // A worker view owns the keys while it is open: esc returns to the fleet from there.
        _ if app.in_worker_view() => app.worker_key(key),
        KeyCode::Char('j') if ctrl => app.insert("\n"),
        KeyCode::Char('v') if ctrl => copy(app, pasted),
        KeyCode::Char('x') if ctrl => app.detach(),
        KeyCode::Char('p') if ctrl => app.recall_previous(),
        KeyCode::Char('n') if ctrl => app.recall_next(),
        KeyCode::Enter if !alt && !shift => submit(app, input),
        KeyCode::Backspace => app.backspace(key),
        KeyCode::PageUp => app.scroll_up(),
        KeyCode::PageDown => app.scroll_down(),
        KeyCode::Home => app.scroll_top(),
        KeyCode::End => app.scroll_bottom(),
        KeyCode::F(2) => app.toggle_workers(),
        KeyCode::F(3) => app.toggle_tool_output(),
        KeyCode::F(4) => app.toggle_worker_view(),
        _ => app.input(key),
    }
}

/// Ctrl+V: the chip goes up at once and the clipboard is read on a thread of its own.
///
/// Reading it on this thread would freeze the frame while `osascript` copies the whole image; the
/// loop applies the answer when it lands.
fn copy(app: &mut App, pasted: &Sender<Copied>) {
    let Some(id) = app.begin_paste() else { return };
    let sender = pasted.clone();
    std::thread::spawn(move || {
        let found = clipboard::read();
        // The loop drops this receiver when it ends, and then there is nobody left to tell.
        let _ = sender.send((id, found));
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::WorkerView;
    use std::path::PathBuf;

    fn press(app: &mut App, code: KeyCode) {
        let (input, queue) = mpsc::channel();
        let (pasted, _reads) = mpsc::channel();
        handle_key(app, &input, &pasted, KeyEvent::from(code));
        assert!(queue.try_recv().is_err(), "a key pressed in a worker view wakes the core");
    }

    /// Esc inside a worker view returns to the fleet: it neither types into the prompt nor quits.
    #[test]
    fn escape_leaves_a_worker_view_without_quitting() {
        let mut app = App::new("m");
        let worker = WorkerView {
            id: 1,
            dir: PathBuf::from("/tmp/repo"),
            task: "do it".into(),
            pending: 0,
            failed: false,
            finished: false,
            busy_for: None,
        };
        app.apply(UiEvent::Workers(vec![worker]));
        press(&mut app, KeyCode::F(4));
        assert!(app.in_worker_view(), "F4 opens a worker's view");
        press(&mut app, KeyCode::Char('x'));
        assert!(app.take_prompt().is_empty(), "no key reaches the prompt from a worker view");
        press(&mut app, KeyCode::Esc);
        assert!(!app.in_worker_view(), "esc returns to the fleet");
        assert!(!app.quit, "esc never quits the app");
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.take_prompt(), "x", "the prompt takes keys again once the view is closed");
    }
}
