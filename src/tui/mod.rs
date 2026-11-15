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
use std::path::Path;
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
    // The layout the last run was left with, and where this one writes its own back.
    let home = crate::config::home().ok();
    if let Some(width) = home.as_deref().and_then(crate::config::panel_width) {
        app.set_workers_width(width);
    }
    let mut terminal = ratatui::try_init()?;
    // ratatui's hook leaves raw mode and the alternate screen; these two are modes of ours.
    let previous = release_modes_on_panic();
    if let Err(err) = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste) {
        // The terminal never entered these modes: bring the app's flag in line and say why.
        app.toggle_mouse();
        app.apply(UiEvent::Error(format!("cannot enable the mouse and paste modes: {err}")));
    }
    let result = event_loop(&mut terminal, &mut app, ui, input, home.as_deref());
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
    home: Option<&Path>,
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
                    // A drag off the panel's rule resizes it; anywhere else the pointer does nothing.
                    MouseEventKind::Drag(MouseButton::Left) => app.drag(mouse.column),
                    MouseEventKind::Up(MouseButton::Left) => app.release(),
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
        if app.take_width_dirty()
            && let Some(home) = home
        {
            crate::config::save_panel_width(home, app.workers_width());
        }
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
    // The rename prompt takes every key while it is open: no binding of the main screen applies.
    if app.renaming() {
        if let Some(event) = app.rename_key(key) {
            let _ = input.send(event);
        }
        return Ok(());
    }
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
        // The verbosity flag is global, like the mouse: both keys work from a worker's view too.
        KeyCode::Char('0' | 'o') if ctrl => app.toggle_tool_output(),
        // A menu in front of the caret takes esc first; a worker's view takes it next, stopping a
        // working agent and leaving when there is nothing to stop. The root is never a target: it
        // is the user's only voice.
        KeyCode::Esc if !app.in_worker_view() && app.completing() => app.close_completion(),
        KeyCode::Esc => match app.stop_target() {
            Some(id) => {
                app.stopping(id);
                let _ = input.send(Event::Stop { id });
            }
            None if app.in_worker_view() => app.worker_key(key),
            None => app.input(key),
        },
        // A worker view owns the keys while it is open: every other key acts inside it.
        _ if app.in_worker_view() => app.worker_key(key),
        // The completion popup owns these keys while it is open, above the composer's own bindings:
        // a menu in front of the caret takes the arrows, and Enter picks from it rather than sending.
        KeyCode::Up if app.completing() => app.step_completion(false),
        KeyCode::Down if app.completing() => app.step_completion(true),
        KeyCode::Tab if app.completing() => {
            app.accept_completion();
        }
        KeyCode::Char('j') if ctrl => app.insert("\n"),
        KeyCode::Char('v') if ctrl => copy(app, pasted),
        KeyCode::Char('x') if ctrl => app.detach(),
        KeyCode::Char('p') if ctrl => app.recall_previous(),
        KeyCode::Char('n') if ctrl => app.recall_next(),
        // A bare `r` types a letter in the prompt, so renaming takes a modifier.
        KeyCode::Char('r') if ctrl => app.start_rename(),
        // Enter accepts the highlighted row; with no popup open it submits, exactly as before.
        KeyCode::Enter if !alt && !shift => {
            if !app.accept_completion() {
                submit(app, input);
            }
        }
        KeyCode::Backspace => app.backspace(key),
        KeyCode::PageUp => app.scroll_up(),
        KeyCode::PageDown => app.scroll_down(),
        KeyCode::Home => app.scroll_top(),
        KeyCode::End => app.scroll_bottom(),
        KeyCode::F(2) => app.toggle_workers(),
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
    use crate::swarm::{AgentView, Progress};
    use std::path::PathBuf;

    /// A key with modifiers, for the bindings that carry one.
    fn press_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        let (input, queue) = mpsc::channel();
        let (pasted, _reads) = mpsc::channel();
        let _ = handle_key(app, &input, &pasted, KeyEvent::new(code, modifiers));
        assert!(queue.try_recv().is_err(), "a key pressed in an agent view wakes the core");
    }

    /// Ctrl+o and ctrl+0 both open every tool card; a bare o or 0 is still typed.
    #[test]
    fn ctrl_o_and_ctrl_0_flip_the_tool_output_flag_and_a_bare_key_does_not() {
        let mut app = App::new("m");
        let before = app.tool_output;
        press_with(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_ne!(app.tool_output, before, "ctrl+o expands or collapses every tool card");
        press_with(&mut app, KeyCode::Char('0'), KeyModifiers::CONTROL);
        assert_eq!(app.tool_output, before, "ctrl+0 toggles the same flag back");
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
            name: crate::names::name(2, None),
            label: crate::names::label(2, None),
            parent: Some(crate::swarm::Swarm::ROOT),
            depth: 1,
            dir: PathBuf::from("/tmp/repo"),
            task: "do it".into(),
            profile: None,
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

    /// A worker with one instruction in flight, as the panel and the view take it.
    fn working(id: usize) -> AgentView {
        AgentView {
            id,
            name: crate::names::name(id, None),
            label: crate::names::label(id, None),
            parent: Some(crate::swarm::Swarm::ROOT),
            depth: 1,
            dir: PathBuf::from("/tmp/repo"),
            task: "do it".into(),
            profile: None,
            pending: 1,
            failed: false,
            finished: false,
            stopped: false,
            paused: false,
            read_only: false,
            busy_for: Some(Duration::from_secs(3)),
        }
    }

    /// Esc on a working agent stops it and everything below it, and its view stays open to show it.
    #[test]
    fn escape_stops_a_working_agent_and_keeps_its_view() {
        let mut app = App::new("m");
        app.apply(UiEvent::Agents(vec![working(2)]));
        press(&mut app, KeyCode::F(4));
        let (input, queue) = mpsc::channel();
        let (pasted, _reads) = mpsc::channel();
        let _ = handle_key(&mut app, &input, &pasted, KeyEvent::from(KeyCode::Esc));
        assert_eq!(queue.try_recv(), Ok(Event::Stop { id: 2 }), "esc stops the agent it is watching");
        let drawn = screen(&mut app, 100, 24);
        assert!(drawn.contains("esc stop · F4 back"), "the header offers the stop: {drawn}");
        assert!(drawn.contains("stopping agent 2…"), "the key is acknowledged at once: {drawn}");
        assert!(app.in_worker_view(), "the view stays open to show the stop land");
        // The core reports it: the agent is stopped, so the next esc is the way out.
        let stopped = AgentView { stopped: true, pending: 0, busy_for: None, ..working(2) };
        app.apply(UiEvent::Agents(vec![stopped]));
        press(&mut app, KeyCode::Esc);
        assert!(!app.in_worker_view(), "esc leaves once there is nothing left to stop");
    }

    /// The root answers the user: no key may end it, since nothing in the program restarts it.
    #[test]
    fn escape_never_stops_the_overseer() {
        let mut app = App::new("m");
        let root = AgentView { parent: None, depth: 0, ..working(crate::swarm::Swarm::ROOT) };
        app.apply(UiEvent::Agents(vec![root]));
        press(&mut app, KeyCode::F(4));
        let drawn = screen(&mut app, 100, 24);
        assert!(drawn.contains("esc back to agents"), "the overseer's view only offers the way out: {drawn}");
        press(&mut app, KeyCode::Esc);
        assert!(!app.in_worker_view(), "esc on the overseer leaves its view");
        assert_eq!(app.stop_target(), None);
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("a test terminal");
        terminal.draw(|frame| app.render(frame)).expect("a frame");
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

    /// The user's request end to end: a fresh app shows a long spawn card on one row, and ctrl+0 opens it.
    #[test]
    fn a_spawn_card_starts_on_one_row_and_ctrl_0_opens_it() {
        let mut app = App::new("m");
        let task = format!("first line\n{}tail", "word ".repeat(40));
        let summary = format!("/tmp · {task}");
        let started = Progress::Started { call: 0, tool: "spawn_agent".into(), summary };
        app.apply(UiEvent::Progress { id: crate::swarm::Swarm::ROOT, progress: started });
        let launched = screen(&mut app, 100, 24);
        assert!(launched.contains("spawn_agent /tmp · first line"), "{launched}");
        assert!(!launched.contains("tail"), "the whole task waits for the key: {launched}");
        assert!(!launched.contains("more lines"), "a compact card counts nothing: {launched}");
        press_with(&mut app, KeyCode::Char('0'), KeyModifiers::CONTROL);
        let opened = screen(&mut app, 100, 24);
        assert!(opened.contains("tail"), "ctrl+0 shows the whole task: {opened}");
    }

    /// Enter with the popup open takes the highlighted row: it never submits what is in the prompt.
    #[test]
    fn enter_accepts_the_popup_instead_of_submitting() {
        let mut app = App::new("m");
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Enter);
        // The composer keeps the space the argument follows in; `take_prompt` trims it.
        assert_eq!(app.take_prompt(), "/resume", "enter took the first row and reached no core");
        assert!(!app.completing(), "the popup closed on the row it accepted");
    }

    /// Down moves the highlight without touching the caret, and tab takes what it landed on.
    #[test]
    fn down_moves_the_highlight_and_tab_accepts_it() {
        let mut app = App::new("m");
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.take_prompt(), "/schemes", "down picked the second row and tab took it");
    }

    /// The popup only takes the keys it lists: ctrl+0, the tool-card toggle, keeps working while the
    /// menu is open, and its `0` never reaches the composer.
    #[test]
    fn ctrl_0_still_toggles_expansion_while_the_popup_is_open() {
        let mut app = App::new("m");
        let before = app.tool_output;
        press(&mut app, KeyCode::Char('/'));
        press_with(&mut app, KeyCode::Char('0'), KeyModifiers::CONTROL);
        assert_ne!(app.tool_output, before, "ctrl+0 is not the popup's key");
        assert!(app.completing(), "and it leaves the menu open");
        assert_eq!(app.take_prompt(), "/", "the 0 never reached the composer");
    }

    /// F3 no longer expands: the two control keys do.
    #[test]
    fn f3_no_longer_toggles_expansion() {
        let mut app = App::new("m");
        let before = app.tool_output;
        press(&mut app, KeyCode::F(3));
        assert_eq!(app.tool_output, before, "F3 leaves the cards as they are");
    }

    /// Esc closes the popup and leaves the typed keys in the composer; it never quits the app.
    #[test]
    fn escape_closes_the_popup_and_keeps_the_typed_text() {
        let mut app = App::new("m");
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Esc);
        assert!(!app.completing(), "esc closes the popup");
        assert!(!app.quit, "esc with the popup open never quits");
        assert_eq!(app.take_prompt(), "/s", "what was typed stays where it was");
    }
}
