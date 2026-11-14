mod app;
mod theme;

use crate::clipboard;
use crate::config::Theme;
use crate::event::UiEvent;
use crate::swarm::Event;
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

/// Core events applied per loop pass: a burst never starves the next draw or a pressed quit.
const BATCH: usize = 256;

/// A clipboard read on its way back, with the id of the paste that asked for it.
type Copied = (u64, clipboard::Paste);

/// Runs the full-screen frontend until the user quits or the core ends.
pub(crate) fn run(ui: Receiver<UiEvent>, input: Sender<Event>, model: &str, theme: Theme) -> Result<()> {
    theme::install(theme);
    let mut app = App::new(model);
    let mut terminal = ratatui::try_init()?;
    // ratatui's hook leaves raw mode and the alternate screen; these two are modes of ours.
    let previous = release_modes_on_panic();
    if let Err(err) = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste) {
        // The terminal never entered these modes: bring the app's flag in line and say why.
        app.toggle_mouse();
        app.apply(UiEvent::Error(format!("cannot enable the mouse and paste modes: {err}")));
    }
    let result = event_loop(&mut terminal, &mut app, ui, input);
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    std::panic::set_hook(Box::new(move |info| previous(info)));
    result
}

/// Restores the two modes `ratatui::restore` does not own, then lets the hook it installed run.
///
/// Without this a panic leaves the terminal reporting the mouse: the shell then prints escape
/// sequences on every move, and dragging selects nothing. Returns the previous hook so the caller
/// can put it back once the terminal is restored.
fn release_modes_on_panic() -> std::sync::Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync> {
    let hook: std::sync::Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync> =
        std::sync::Arc::from(std::panic::take_hook());
    let inner = std::sync::Arc::clone(&hook);
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        inner(info);
    }));
    hook
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
                Input::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, &input, &pasted, key)?,
                Input::Paste(text) => app.paste(&text),
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
        // A busy core can stream faster than this loop empties; a bounded batch keeps the next
        // draw, and a just-pressed quit, from waiting on the whole burst.
        for _ in 0..BATCH {
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

fn handle_key(app: &mut App, input: &Sender<Event>, pasted: &Sender<Copied>, key: KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('c') if ctrl => quit(app, input),
        KeyCode::Char('d') if ctrl => {
            let _ = input.send(Event::Eof);
            app.drain();
        }
        // The mouse belongs to the terminal, not to a view: F5 works from a worker's view too.
        KeyCode::F(5) => mouse(app)?,
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
        // The same verbosity flag, for a keyboard without function keys: F3 expanded everything.
        KeyCode::Char('o') if ctrl => app.toggle_tool_output(),
        KeyCode::F(4) => app.toggle_worker_view(),
        _ => app.input(key),
    }
    Ok(())
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

/// F5: captures the mouse for clicks and the wheel, or hands it back so the terminal's own
/// selection and copy work again.
fn mouse(app: &mut App) -> Result<()> {
    if app.toggle_mouse() {
        execute!(stdout(), EnableMouseCapture)?;
    } else {
        execute!(stdout(), DisableMouseCapture)?;
    }
    Ok(())
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
    if let Some(event) = Event::from_line(&prompt) {
        if matches!(event, Event::Exit) {
            quit(app, input);
        } else if let Err(err) = input.send(event) {
            app.apply(UiEvent::Error(format!("cannot reach the core: {err}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::AgentView;
    use std::path::PathBuf;

    /// A key with modifiers, for the bindings that carry one.
    fn press_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        let (input, queue) = mpsc::channel();
        let (pasted, _reads) = mpsc::channel();
        let _ = handle_key(app, &input, &pasted, KeyEvent::new(code, modifiers));
        assert!(queue.try_recv().is_err(), "a key pressed in an agent view wakes the core");
    }

    /// Ctrl+O is F3's toggle without the function keys; a bare o is still a letter.
    #[test]
    fn ctrl_o_flips_the_tool_output_flag_and_a_bare_o_does_not() {
        let mut app = App::new("m");
        let before = app.tool_output;
        press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_ne!(app.tool_output, before, "ctrl+o expands or collapses every tool card");
        press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(app.tool_output, before, "the same key toggles it back");
        press_with(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(app.tool_output, before, "a bare o never flips the flag");
        assert_eq!(app.take_prompt(), "o", "a bare o types an o into the composer");
    }

    fn press(app: &mut App, code: KeyCode) {
        press_with(app, code, KeyModifiers::NONE);
    }

    /// Esc inside an agent view returns to the tree: it neither types into the prompt nor quits.
    #[test]
    fn escape_leaves_an_agent_view_without_quitting() {
        let mut app = App::new("m");
        let agent = AgentView {
            id: 2,
            parent: Some(crate::swarm::Swarm::ROOT),
            depth: 1,
            dir: PathBuf::from("/tmp/repo"),
            task: "do it".into(),
            pending: 0,
            failed: false,
            finished: false,
            stopped: false,
            paused: false,
            read_only: false,
            busy_for: None,
        };
        app.apply(UiEvent::Agents(vec![agent]));
        press(&mut app, KeyCode::F(4));
        assert!(app.in_worker_view(), "F4 opens an agent's view");
        press(&mut app, KeyCode::Char('x'));
        assert!(app.take_prompt().is_empty(), "no key reaches the prompt from an agent view");
        press(&mut app, KeyCode::Esc);
        assert!(!app.in_worker_view(), "esc returns to the tree");
        assert!(!app.quit, "esc never quits the app");
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.take_prompt(), "x", "the prompt takes keys again once the view is closed");
    }
}
