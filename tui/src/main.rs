//! `ccwatch` — the terminal client. Auto-spawns and subscribes to `ccwatchd`,
//! renders live snapshots, and drives actions with confirmation.

mod app;
mod client;
mod format;
mod ui;

use app::{ActionKind, App, Mode};
use ccwatch_core::Paths;
use client::FromDaemon;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let paths = Paths::discover();

    if let Err(e) = client::ensure_daemon(&paths) {
        eprintln!("warning: {e}");
    }
    let rx = client::subscribe(&paths).ok();

    let mut app = App::new(chrono::Utc::now().timestamp_millis());
    app.connected = rx.is_some();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &paths, rx);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    paths: &Paths,
    mut rx: Option<Receiver<FromDaemon>>,
) -> anyhow::Result<()> {
    let mut last_reconnect = Instant::now();
    // Draw only when something changed (snapshot, keypress, connection) or on
    // a 1 s heartbeat for the relative-time labels — an idle TUI costs ~0 CPU.
    let mut dirty = true;
    let mut last_periodic = Instant::now();
    loop {
        // Drain any pending daemon messages.
        if let Some(r) = &rx {
            loop {
                match r.try_recv() {
                    Ok(FromDaemon::Snapshot(s)) => {
                        app.connected = true;
                        app.set_snapshot(*s);
                        dirty = true;
                    }
                    Ok(FromDaemon::Heartbeat) => app.connected = true,
                    Ok(FromDaemon::Disconnected) => {
                        app.connected = false;
                        rx = None;
                        dirty = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        app.connected = false;
                        rx = None;
                        dirty = true;
                        break;
                    }
                }
            }
        }

        // Try to (re)connect if we have no live subscription.
        if rx.is_none() && last_reconnect.elapsed() > Duration::from_secs(2) {
            last_reconnect = Instant::now();
            let _ = client::ensure_daemon(paths);
            if let Ok(new_rx) = client::subscribe(paths) {
                rx = Some(new_rx);
                app.connected = true;
                dirty = true;
            }
        }

        if last_periodic.elapsed() >= Duration::from_secs(1) {
            last_periodic = Instant::now();
            dirty = true; // "Ns ago" labels tick forward
        }

        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(app, paths, key.code);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut App, paths: &Paths, code: KeyCode) {
    match &app.mode {
        Mode::Fuzzy { .. } => match code {
            KeyCode::Esc => app.cancel_mode(),
            KeyCode::Enter => app.fuzzy_commit(),
            KeyCode::Backspace => app.fuzzy_backspace(),
            KeyCode::Up => app.fuzzy_move(-1),
            KeyCode::Down => app.fuzzy_move(1),
            KeyCode::Char(c) => app.fuzzy_input(c),
            _ => {}
        },
        Mode::Confirm(_) => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(req) = app.take_pending() {
                    let (ok, msg) = client::send_action(paths, req);
                    app.status = Some(format!("{} {msg}", if ok { "✓" } else { "✗" }));
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.cancel_mode(),
            _ => {}
        },
        Mode::Details => match code {
            KeyCode::Esc | KeyCode::Char('d') | KeyCode::Char('q') | KeyCode::Enter => {
                app.cancel_mode()
            }
            _ => {}
        },
        Mode::Normal => match code {
            KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
            KeyCode::Char('/') => app.open_fuzzy(),
            KeyCode::Char('d') => {
                if app.selected_row().is_some() {
                    app.mode = Mode::Details;
                }
            }
            KeyCode::Char('x') => {
                app.hide_done = !app.hide_done;
                app.move_selection(0);
            }
            KeyCode::Char('s') => {
                app.sort = app.sort.next();
                app.status = Some(format!("sorted by {}", app.sort.label()));
            }
            KeyCode::Up => app.move_selection(-1),
            KeyCode::Down => app.move_selection(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Left | KeyCode::Char(' ') => {
                app.toggle_expand()
            }
            KeyCode::Char('k') => app.stage_action(ActionKind::Kill),
            KeyCode::Char('p') => app.stage_action(ActionKind::Pause),
            KeyCode::Char('r') => app.stage_action(ActionKind::Resume),
            KeyCode::Char('C') => app.stage_apply_pacing(),
            KeyCode::Char('X') => app.stage_release_cruise(),
            KeyCode::Char('f') => {
                app.hide_idle = !app.hide_idle;
                app.move_selection(0); // clamp selection to new list length
            }
            _ => {}
        },
    }
}
