//! `ccwatch-menubar` — Phase 3 macOS menu-bar client.
//!
//! Same daemon and IPC as the TUI. The menu-bar icon is a live load graph
//! (rendered 2× for Retina; see [`graph`]), the title next to it is the current
//! burn rate (or `⚠N` when leaks are detected), and the dropdown lists alerts
//! and per-session submenus with pause / resume / kill actions (destructive
//! ones confirmed via a native dialog). Reconnects automatically if the daemon
//! dies.
//!
//! `ccwatch-menubar --dump` prints the title/tooltip/menu once and exits — a
//! headless way to verify the pipeline without a GUI session.

mod client;
mod graph;
mod prefs;
mod summary;

use ccwatch_core::Paths;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let paths = Paths::discover();

    if std::env::args().any(|a| a == "--dump") {
        client::ensure_daemon(&paths)?;
        let snap = client::latest_snapshot(&paths, Duration::from_secs(5), Duration::from_secs(3))?;
        let model = summary::menu_model(&snap);
        println!("title:   {}", summary::tray_title(&snap, true, prefs::TitleMode::Throttle));
        println!("tooltip: {}", summary::tooltip(&snap));
        println!("gov:     {}", summary::governor_line(&snap));
        let rates: Vec<f64> = snap.sessions.iter().map(|s| s.tokens_per_min).collect();
        println!("load:    {}  (per-session tok/min)", graph::unicode_spark(&rates));
        println!("menu:");
        println!("  {}", model.header);
        for a in &model.alerts {
            println!("  {a}");
        }
        for s in &model.sessions {
            println!("  ▸ {}", s.title);
            println!("      ∿ {}", s.tokens_line);
            for i in &s.info {
                println!("      {i}");
            }
            println!("      [{}]{}", s.kill_label, if s.can_pause { " [Pause] [Resume]" } else { "" });
        }
        return Ok(());
    }

    run(&paths)
}

#[cfg(target_os = "macos")]
fn run(paths: &Paths) -> anyhow::Result<()> {
    macos::run(paths)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{client, graph, summary};
    use ccwatch_core::ipc::ActionRequest;
    use ccwatch_core::{Config, Paths};
    use std::collections::HashMap;
    use std::sync::mpsc::{Receiver, TryRecvError};
    use std::time::{Duration, Instant};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use crate::prefs::{Prefs, TitleMode};
    use tray_icon::menu::{
        CheckMenuItem, Icon as MenuIcon, IconMenuItem, Menu, MenuEvent, MenuId, MenuItem,
        PredefinedMenuItem, Submenu,
    };
    use tray_icon::TrayIconBuilder;

    /// Minimum spacing between graph samples, so the ~45 s window is stable
    /// regardless of how often the daemon pushes.
    const SAMPLE_EVERY: Duration = Duration::from_millis(1500);

    /// What a clickable menu item does. Rebuilt from the latest model on every
    /// update, so payloads are never stale.
    #[derive(Clone)]
    enum Action {
        Quit,
        OpenTui,
        ToggleHideIdle,
        SetTitleMode(TitleMode),
        Pause { pid: i32, name: String },
        Resume { pid: i32, name: String },
        Kill { pid: i32, name: String },
        Cancel { remote: String, id: String, name: String },
    }

    /// The live menu items for one session's submenu. The first row is an
    /// iStat-style area sparkline of that session's burn with the token
    /// breakdown beside it.
    struct SessionRow {
        submenu: Submenu,
        spark: IconMenuItem,
        info: [MenuItem; 2],
        pause: MenuItem,
        resume: MenuItem,
        kill: MenuItem,
    }

    impl SessionRow {
        fn new(entry: &summary::SessionEntry, icon: Option<MenuIcon>) -> Self {
            let submenu = Submenu::new(&entry.title, true);
            let spark = IconMenuItem::new(&entry.tokens_line, true, icon, None);
            let info = [
                MenuItem::new(&entry.info[0], false, None),
                MenuItem::new(&entry.info[1], false, None),
            ];
            let pause = MenuItem::new("Pause (SIGSTOP)", entry.can_pause, None);
            let resume = MenuItem::new("Resume (SIGCONT)", entry.can_pause, None);
            let kill = MenuItem::new(
                &entry.kill_label,
                entry.action != summary::SessionAction::None,
                None,
            );
            let _ = submenu.append(&spark);
            let _ = submenu.append(&PredefinedMenuItem::separator());
            for i in &info {
                let _ = submenu.append(i);
            }
            let _ = submenu.append(&PredefinedMenuItem::separator());
            let _ = submenu.append(&pause);
            let _ = submenu.append(&resume);
            let _ = submenu.append(&kill);
            SessionRow {
                submenu,
                spark,
                info,
                pause,
                resume,
                kill,
            }
        }

        fn update(&self, entry: &summary::SessionEntry, icon: Option<MenuIcon>) {
            self.submenu.set_text(&entry.title);
            self.spark.set_text(&entry.tokens_line);
            self.spark.set_icon(icon);
            for (item, text) in self.info.iter().zip(&entry.info) {
                item.set_text(text);
            }
            self.pause.set_enabled(entry.can_pause);
            self.resume.set_enabled(entry.can_pause);
            self.kill.set_text(&entry.kill_label);
            self.kill
                .set_enabled(entry.action != summary::SessionAction::None);
        }
    }

    /// Owns the dropdown and diffs it against each new [`summary::MenuModel`]:
    /// stable items update in place; rows are inserted/removed only when counts
    /// change. No blank filler rows, ever.
    ///
    /// Layout: `[header][separator][alerts…][sessions…][separator][Open TUI][Quit]`.
    struct TrayMenu {
        menu: Menu,
        header: MenuItem,
        governor: MenuItem,
        alerts: Vec<MenuItem>,
        sessions: Vec<SessionRow>,
        open_tui: MenuItem,
        quit: MenuItem,
        hide_idle: CheckMenuItem,
        mode_items: Vec<(TitleMode, CheckMenuItem)>,
    }

    /// Index of the first dynamic row (after header + governor + separator).
    const DYN_BASE: usize = 3;

    impl TrayMenu {
        fn new(prefs: &Prefs) -> Self {
            let menu = Menu::new();
            let header = MenuItem::new("connecting…", false, None);
            let governor = MenuItem::new("governor: no data", false, None);
            let open_tui = MenuItem::new("Open TUI dashboard", true, None);
            let quit = MenuItem::new("Quit ccwatch", true, None);

            // Settings ▸ hide-idle toggle + bar-content radio.
            let settings = Submenu::new("Settings", true);
            let hide_idle = CheckMenuItem::new("Hide idle sessions", true, prefs.hide_idle, None);
            let _ = settings.append(&hide_idle);
            let _ = settings.append(&PredefinedMenuItem::separator());
            let bar_label = MenuItem::new("Show in menu bar:", false, None);
            let _ = settings.append(&bar_label);
            let mode_items: Vec<(TitleMode, CheckMenuItem)> = TitleMode::ALL
                .iter()
                .map(|m| {
                    let item =
                        CheckMenuItem::new(m.label(), true, *m == prefs.title_mode, None);
                    let _ = settings.append(&item);
                    (*m, item)
                })
                .collect();

            let _ = menu.append(&header);
            let _ = menu.append(&governor);
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&settings);
            let _ = menu.append(&open_tui);
            let _ = menu.append(&quit);
            TrayMenu {
                menu,
                header,
                governor,
                alerts: Vec::new(),
                sessions: Vec::new(),
                open_tui,
                quit,
                hide_idle,
                mode_items,
            }
        }

        /// Sync the Settings checkmarks with the prefs.
        fn sync_prefs(&self, prefs: &Prefs) {
            self.hide_idle.set_checked(prefs.hide_idle);
            for (mode, item) in &self.mode_items {
                item.set_checked(*mode == prefs.title_mode);
            }
        }

        fn apply(
            &mut self,
            model: &summary::MenuModel,
            spark_icon: &dyn Fn(&summary::SessionEntry) -> Option<MenuIcon>,
        ) -> HashMap<MenuId, Action> {
            self.header.set_text(&model.header);

            // Alerts: update in place, then grow/shrink.
            let common = self.alerts.len().min(model.alerts.len());
            for i in 0..common {
                self.alerts[i].set_text(&model.alerts[i]);
            }
            for i in self.alerts.len()..model.alerts.len() {
                let item = MenuItem::new(&model.alerts[i], true, None);
                let _ = self.menu.insert(&item, DYN_BASE + i);
                self.alerts.push(item);
            }
            while self.alerts.len() > model.alerts.len() {
                let item = self.alerts.pop().unwrap();
                let _ = self.menu.remove(&item);
            }

            // Sessions: same diffing, offset by the (new) alert count.
            let base = DYN_BASE + self.alerts.len();
            let common = self.sessions.len().min(model.sessions.len());
            for i in 0..common {
                self.sessions[i].update(&model.sessions[i], spark_icon(&model.sessions[i]));
            }
            for i in self.sessions.len()..model.sessions.len() {
                let entry = &model.sessions[i];
                let row = SessionRow::new(entry, spark_icon(entry));
                let _ = self.menu.insert(&row.submenu, base + i);
                self.sessions.push(row);
            }
            while self.sessions.len() > model.sessions.len() {
                let row = self.sessions.pop().unwrap();
                let _ = self.menu.remove(&row.submenu);
            }

            // Fresh action map with never-stale payloads.
            let mut map = HashMap::new();
            map.insert(self.quit.id().clone(), Action::Quit);
            map.insert(self.open_tui.id().clone(), Action::OpenTui);
            map.insert(self.hide_idle.id().clone(), Action::ToggleHideIdle);
            for (mode, item) in &self.mode_items {
                map.insert(item.id().clone(), Action::SetTitleMode(*mode));
            }
            for (row, entry) in self.sessions.iter().zip(&model.sessions) {
                if let summary::SessionAction::Signal { pid } = entry.action {
                    map.insert(
                        row.pause.id().clone(),
                        Action::Pause { pid, name: entry.name.clone() },
                    );
                    map.insert(
                        row.resume.id().clone(),
                        Action::Resume { pid, name: entry.name.clone() },
                    );
                    map.insert(
                        row.kill.id().clone(),
                        Action::Kill { pid, name: entry.name.clone() },
                    );
                } else if let summary::SessionAction::Cancel { remote, id } = &entry.action {
                    map.insert(
                        row.kill.id().clone(),
                        Action::Cancel {
                            remote: remote.clone(),
                            id: id.clone(),
                            name: entry.name.clone(),
                        },
                    );
                }
            }
            map
        }
    }

    /// Native confirmation dialog. Returns true when the user clicks `verb`.
    fn confirm(message: &str, verb: &str) -> bool {
        let msg = message.replace('"', "'");
        let script = format!(
            "display dialog \"{msg}\" buttons {{\"Cancel\", \"{verb}\"}} \
             default button \"Cancel\" cancel button \"Cancel\" with icon caution \
             with title \"ccwatch\""
        );
        std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn notify(message: &str) {
        let msg = message.replace('"', "'");
        let script = format!("display notification \"{msg}\" with title \"ccwatch\"");
        let _ = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .output();
    }

    /// Run a confirmed destructive action off the UI thread.
    fn run_action(paths: Paths, prompt: String, verb: &'static str, req: ActionRequest) {
        std::thread::spawn(move || {
            if !confirm(&prompt, verb) {
                return;
            }
            let (ok, msg) = client::send_action(&paths, req);
            notify(&format!("{}{msg}", if ok { "" } else { "failed: " }));
        });
    }

    /// The user's terminal: config override first, then the first installed
    /// of the common terminals, then macOS Terminal.
    fn terminal_app(configured: &str) -> String {
        if !configured.is_empty() {
            return configured.to_string();
        }
        for candidate in ["iTerm", "Ghostty", "WezTerm", "Warp", "Alacritty", "kitty"] {
            if std::path::Path::new(&format!("/Applications/{candidate}.app")).exists() {
                return candidate.to_string();
            }
        }
        "Terminal".to_string()
    }

    fn open_tui(terminal: &str) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let tui = dir.join("ccwatch");
                if tui.exists() {
                    let _ = std::process::Command::new("/usr/bin/open")
                        .args(["-a", terminal])
                        .arg(tui)
                        .spawn();
                    return;
                }
            }
        }
        notify("ccwatch binary not found next to ccwatch-menubar");
    }

    pub fn run(paths: &Paths) -> anyhow::Result<()> {
        let _ = client::ensure_daemon(paths);
        let mut rx: Option<Receiver<ccwatch_core::model::Snapshot>> =
            client::subscribe(paths).ok();
        let cfg = Config::load(&paths.config_file());
        let burn = cfg.burn_tokens_per_min;
        let terminal = terminal_app(&cfg.terminal_app);
        let prefs_path = paths.ccwatch_dir().join("menubar.json");
        let mut prefs = Prefs::load(&prefs_path);

        let event_loop = EventLoopBuilder::new().build();
        let mut tray_menu = TrayMenu::new(&prefs);
        let mut actions: HashMap<MenuId, Action> = HashMap::new();
        actions.insert(tray_menu.quit.id().clone(), Action::Quit);
        actions.insert(tray_menu.open_tui.id().clone(), Action::OpenTui);

        let mut history = graph::History::new(graph::SLOTS);
        // Per-session sparkline histories, keyed by session id.
        let mut session_hist: HashMap<String, graph::History> = HashMap::new();
        let initial = graph::render_tray(&[], burn);
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu.menu.clone()))
            .with_icon(tray_icon::Icon::from_rgba(
                initial,
                graph::ICON_W as u32,
                graph::ICON_H as u32,
            )?)
            .with_title("…")
            .with_tooltip("ccwatch — connecting")
            .build()?;

        let menu_channel = MenuEvent::receiver();
        let paths = paths.clone();
        let mut last_sample = Instant::now() - SAMPLE_EVERY;
        let mut last_reconnect = Instant::now();
        let mut last_snap: Option<ccwatch_core::model::Snapshot> = None;
        // Render only when data or prefs changed — never on idle loop passes.
        let mut render_needed = false;

        event_loop.run(move |_event, _target, control_flow| {
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));

            // Drain daemon messages, keeping only the newest snapshot.
            let mut latest = None;
            if let Some(r) = &rx {
                loop {
                    match r.try_recv() {
                        Ok(snap) => latest = Some(snap),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            rx = None;
                            tray.set_title(Some("⏻"));
                            let _ = tray.set_tooltip(Some("ccwatch — daemon disconnected, reconnecting…"));
                            tray_menu.header.set_text("daemon disconnected — reconnecting…");
                            break;
                        }
                    }
                }
            }

            // Reconnect if needed (spawns the daemon when it's gone).
            if rx.is_none() && last_reconnect.elapsed() > Duration::from_secs(2) {
                last_reconnect = Instant::now();
                let _ = client::ensure_daemon(&paths);
                if let Ok(new_rx) = client::subscribe(&paths) {
                    rx = Some(new_rx);
                }
            }

            if latest.is_some() {
                last_snap = latest;
                render_needed = true;
            }
            if render_needed && last_snap.is_some() {
                render_needed = false;
                let snap = last_snap.clone().unwrap();
                // Prefs filter: idle sessions can be hidden from the dropdown.
                let mut view = snap.clone();
                if prefs.hide_idle {
                    // Keep anything still burning — "idle" state lags behind
                    // long tool calls that emit no messages while working.
                    view.sessions.retain(|s| {
                        !matches!(s.state, ccwatch_core::model::SessionState::Idle)
                            || s.tokens_per_min >= 1.0
                    });
                }
                let model = summary::menu_model(&view);
                if last_sample.elapsed() >= SAMPLE_EVERY {
                    last_sample = Instant::now();
                    history.push(snap.totals.tokens_per_min);
                    let rgba = graph::render_tray(&history.values(), burn);
                    if let Ok(icon) = tray_icon::Icon::from_rgba(
                        rgba,
                        graph::ICON_W as u32,
                        graph::ICON_H as u32,
                    ) {
                        let _ = tray.set_icon(Some(icon));
                    }
                    // Advance per-session sparkline histories; drop gone sessions.
                    for entry in &model.sessions {
                        session_hist
                            .entry(entry.id.clone())
                            .or_insert_with(|| graph::History::new(graph::SPARK_SLOTS))
                            .push(entry.tokens_per_min);
                    }
                    let live: std::collections::HashSet<&str> =
                        model.sessions.iter().map(|e| e.id.as_str()).collect();
                    session_hist.retain(|id, _| live.contains(id.as_str()));
                }
                tray.set_title(Some(summary::tray_title(&view, true, prefs.title_mode)));
                let _ = tray.set_tooltip(Some(summary::tooltip(&snap)));
                tray_menu.governor.set_text(summary::governor_line(&snap));
                actions = tray_menu.apply(&model, &|entry| {
                    let hist = session_hist.get(&entry.id)?;
                    let rgba = graph::render_spark(&hist.values(), burn);
                    MenuIcon::from_rgba(rgba, graph::SPARK_W as u32, graph::SPARK_H as u32).ok()
                });
            }

            // Menu clicks.
            while let Ok(ev) = menu_channel.try_recv() {
                match actions.get(&ev.id).cloned() {
                    Some(Action::Quit) => *control_flow = ControlFlow::Exit,
                    Some(Action::OpenTui) => open_tui(&terminal),
                    Some(Action::ToggleHideIdle) => {
                        prefs.hide_idle = !prefs.hide_idle;
                        prefs.save(&prefs_path);
                        tray_menu.sync_prefs(&prefs);
                        render_needed = true;
                    }
                    Some(Action::SetTitleMode(mode)) => {
                        prefs.title_mode = mode;
                        prefs.save(&prefs_path);
                        tray_menu.sync_prefs(&prefs);
                        render_needed = true;
                    }
                    Some(Action::Pause { pid, name }) => {
                        let (ok, msg) =
                            client::send_action(&paths, ActionRequest::PauseSession { pid });
                        notify(&if ok { format!("paused {name}") } else { msg });
                    }
                    Some(Action::Resume { pid, name }) => {
                        let (ok, msg) =
                            client::send_action(&paths, ActionRequest::ResumeSession { pid });
                        notify(&if ok { format!("resumed {name}") } else { msg });
                    }
                    Some(Action::Kill { pid, name }) => run_action(
                        paths.clone(),
                        format!("Kill session \"{name}\" (pid {pid})?\nSIGTERM, then SIGKILL if it survives."),
                        "Kill",
                        ActionRequest::KillSession { pid },
                    ),
                    Some(Action::Cancel { remote, id, name }) => run_action(
                        paths.clone(),
                        format!("Cancel \"{name}\" on {remote}?\nRuns the host's configured cancel command."),
                        "Cancel it",
                        ActionRequest::CancelRemote { remote, id },
                    ),
                    None => {}
                }
            }
        });
    }
}

#[cfg(not(target_os = "macos"))]
fn run(_paths: &Paths) -> anyhow::Result<()> {
    anyhow::bail!("the menu-bar client is macOS-only; use `--dump` or the TUI on this platform")
}
