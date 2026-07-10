//! `ccwatchd` — the always-on collector daemon.
//!
//! It owns the single [`Engine`], refreshes it on file-change events (with a
//! poll backstop), and serves newline-delimited JSON over a Unix domain socket:
//! subscribers get pushed snapshots; action requests are executed and answered.
//!
//! `ccwatchd --once` prints a single snapshot as JSON and exits — handy for
//! scripting and verification.

mod remotes;
mod usage_bridge;

use ccwatch_core::actions::{self, ActionOutcome};
use ccwatch_core::ipc::{ActionRequest, ClientMsg, ServerMsg};
use ccwatch_core::model::{PaceTarget, Priority, Snapshot};
use ccwatch_core::remote::{self, RemoteDef, SystemRunner};
use ccwatch_core::{Config, Engine, Paths};
use remotes::RemoteManager;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(1500);
/// Exit this long after the last subscriber disconnects (override with
/// `CCWATCH_IDLE_EXIT_SECS`; `--persist` disables).
const IDLE_EXIT: Duration = Duration::from_secs(15);
const PUSH_INTERVAL: Duration = Duration::from_millis(500);
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How often to re-fetch remote hosts (override with `CCWATCH_REMOTE_SECS`).
const DEFAULT_REMOTE_SECS: u64 = 15;

type SharedSnapshot = Arc<RwLock<Arc<Snapshot>>>;

/// Runtime state for Cruise Control's autonomous enforcement: the live mode
/// (only "auto" ever pauses anything) and the set of pids Cruise itself has
/// paused (so it — and only it — can release them).
#[derive(Default)]
struct CruiseRuntime {
    mode: String, // "off" | "advisory" | "oneclick" | "auto"
    paced: std::collections::HashSet<PaceTarget>,
    /// User's per-session priority overrides, keyed by stable session id. Only
    /// High/Low are stored (Normal = no override). Stamped onto each snapshot's
    /// sessions and persisted so pins survive restarts.
    overrides: std::collections::HashMap<String, Priority>,
    #[allow(dead_code)]
    last_log: Option<String>,
}

type SharedCruise = Arc<std::sync::Mutex<CruiseRuntime>>;

/// Pause a target with the actuator its host requires: a local `kill(pid,SIGSTOP)`
/// when `ssh` is None, else `ssh <target> kill -STOP <pid>` on the remote host.
/// A remote pid is thus never signalled as a local pid.
fn do_pause(t: &PaceTarget) -> bool {
    match &t.ssh {
        None => matches!(actions::pause(t.pid), ActionOutcome::Ok(_)),
        Some(h) => matches!(actions::pause_remote(h, t.pid), ActionOutcome::Ok(_)),
    }
}

/// Resume a target (local SIGCONT or `ssh … kill -CONT`).
fn do_resume(t: &PaceTarget) -> bool {
    match &t.ssh {
        None => matches!(actions::resume(t.pid), ActionOutcome::Ok(_)),
        Some(h) => matches!(actions::resume_remote(h, t.pid), ActionOutcome::Ok(_)),
    }
}

/// Is a target's process still alive? (Local `kill -0`, or `ssh … kill -0`.)
fn do_alive(t: &PaceTarget) -> bool {
    match &t.ssh {
        None => actions::alive(t.pid),
        Some(h) => actions::alive_remote(h, t.pid),
    }
}

/// Path of the persisted set of Cruise-paced targets. Read on startup (to release
/// orphans a crashed daemon left stopped) and rewritten whenever `paced` changes.
fn cruise_paced_file(paths: &Paths) -> std::path::PathBuf {
    paths.ccwatch_dir().join("cruise-paced.json")
}

/// Path of the persisted per-session priority overrides (session id → tier).
fn cruise_priority_file(paths: &Paths) -> std::path::PathBuf {
    paths.ccwatch_dir().join("cruise-priority.json")
}

/// Persist the priority-override map (best-effort) so pins survive a restart.
fn persist_overrides(paths: &Paths, overrides: &std::collections::HashMap<String, Priority>) {
    if let Ok(json) = serde_json::to_string(overrides) {
        let _ = std::fs::write(cruise_priority_file(paths), json);
    }
}

/// Load the persisted priority-override map on startup (empty if absent/corrupt).
fn load_overrides(paths: &Paths) -> std::collections::HashMap<String, Priority> {
    std::fs::read_to_string(cruise_priority_file(paths))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Persist the current `paced` set (best-effort; errors ignored). Called whenever
/// the set changes so a restart's startup sweep can release exactly these pids.
fn persist_paced(paths: &Paths, paced: &std::collections::HashSet<PaceTarget>) {
    let targets: Vec<PaceTarget> = paced.iter().cloned().collect();
    if let Ok(json) = serde_json::to_string(&targets) {
        let _ = std::fs::write(cruise_paced_file(paths), json);
    }
}

/// Startup safety sweep: resume every pid a previous daemon left paused, so a
/// crash/restart never leaves a live session frozen. Returns the count resumed.
/// The persisted set is cleared afterward; if auto mode is still on, the next
/// refresher tick re-pauses as needed (a brief gap is acceptable; a stuck
/// session is not).
fn sweep_orphaned_paced(paths: &Paths) -> usize {
    let path = cruise_paced_file(paths);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return 0; // no file → nothing to sweep (fresh install / clean shutdown)
    };
    let targets: Vec<PaceTarget> = serde_json::from_str(&text).unwrap_or_default();
    let mut resumed = 0usize;
    for t in &targets {
        if do_resume(t) {
            resumed += 1;
        }
    }
    let _ = std::fs::write(&path, "[]"); // start clean regardless of resume outcome
    resumed
}

fn main() -> anyhow::Result<()> {
    let paths = Paths::discover();

    if std::env::args().any(|a| a == "--once") {
        let mut engine = Engine::with_defaults();
        let snap = engine.refresh_now();
        println!("{}", serde_json::to_string_pretty(&snap)?);
        return Ok(());
    }

    std::fs::create_dir_all(paths.ccwatch_dir())?;

    // Refuse to double-launch only if a daemon actually answers the socket —
    // a live pid alone can be a reused pid with a stale pidfile.
    if std::os::unix::net::UnixStream::connect(paths.socket()).is_ok() {
        if let Some(pid) = read_pidfile(&paths) {
            eprintln!("ccwatchd already running (pid {pid})");
        } else {
            eprintln!("ccwatchd socket already active");
        }
        return Ok(());
    }
    let _ = std::fs::remove_file(paths.socket()); // clear any stale socket
    std::fs::write(paths.pidfile(), std::process::id().to_string())?;

    // Fresh socket.
    let sock = paths.socket();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    eprintln!("ccwatchd listening at {}", sock.display());

    // The daemon exists to serve clients: count subscribers and exit once the
    // last one has been gone for a grace period. A surviving client (TUI or
    // menu bar) keeps it alive; `--persist` opts out entirely.
    let subscribers = Arc::new(AtomicUsize::new(0));

    // Shared latest snapshot, updated by the refresher thread.
    let shared: SharedSnapshot = Arc::new(RwLock::new(Arc::new(Snapshot::empty(0))));

    // Startup safety sweep FIRST: resume any pids a previous daemon left paused,
    // so a crash/restart never leaves a live session frozen. Then start with an
    // empty `paced` set (the file is cleared by the sweep).
    let swept = sweep_orphaned_paced(&paths);
    if swept > 0 {
        eprintln!("cruise: resumed {swept} orphaned paused session(s) from a previous run");
    }

    // Cruise Control runtime state: default OFF unless config says otherwise.
    // Seeded from config now; updated at runtime only via `SetCruiseMode`.
    let cruise_config = Config::load(&paths.config_file());
    let cruise: SharedCruise = Arc::new(std::sync::Mutex::new(CruiseRuntime {
        // Normalize the same way `SetCruiseMode` does, so `cruise_mode = "Auto"`
        // in config.toml engages auto instead of silently acting as off.
        mode: cruise_config.cruise_mode.trim().to_lowercase(),
        overrides: load_overrides(&paths),
        ..Default::default()
    }));

    // Idle self-exit: if no client subscribes for `grace`, shut down — but NEVER
    // leave a session frozen. Resume everything Cruise paused and clear the record
    // first, so a SIGSTOP'd session can't be stranded until some future daemon
    // launch happens to run the startup sweep (a headless paused session can't
    // trigger that launch). Spawned AFTER `cruise` exists so it can reach it.
    if !std::env::args().any(|a| a == "--persist") {
        let grace = std::env::var("CCWATCH_IDLE_EXIT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(IDLE_EXIT);
        let subs = subscribers.clone();
        let sock = sock.clone();
        let pidfile = paths.pidfile();
        let cruise = cruise.clone();
        let cruise_paths = paths.clone();
        std::thread::spawn(move || {
            let mut last_active = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                if subs.load(Ordering::Relaxed) > 0 {
                    last_active = Instant::now();
                } else if last_active.elapsed() > grace {
                    eprintln!("no clients for {grace:?}; exiting");
                    // Release before exit: resume every Cruise-paused target so
                    // none stay frozen with no running daemon to sweep them.
                    {
                        let mut c = cruise.lock().unwrap();
                        for t in c.paced.iter() {
                            let _ = do_resume(t);
                        }
                        c.paced.clear();
                        persist_paced(&cruise_paths, &c.paced);
                    }
                    let _ = std::fs::remove_file(&sock);
                    let _ = std::fs::remove_file(&pidfile);
                    std::process::exit(0);
                }
            }
        });
    }

    // Remote/cloud hosts: fetched on their own cadence, cached, merged in.
    let remote_defs = remote::load_remotes(&paths.remotes_file());
    if !remote_defs.is_empty() {
        eprintln!("tracking {} remote host(s)", remote_defs.len());
    }
    let manager = RemoteManager::new(remote_defs);
    let remote_cache = manager.cache();
    let remote_defs = manager.defs();
    let remote_secs = std::env::var("CCWATCH_REMOTE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REMOTE_SECS);
    manager.spawn(Duration::from_secs(remote_secs));

    // A tick channel drives refreshes: poll timer + file-change events both send.
    let (tick_tx, tick_rx) = mpsc::channel::<()>();
    spawn_poll_timer(tick_tx.clone());
    let _watcher = spawn_fs_watcher(&paths, tick_tx.clone());

    // Refresher thread owns the engine and merges cached remote snapshots.
    // Failing remotes surface as RemoteDown alerts rather than vanishing, and
    // the Governor (fuel gauge) is computed over the merged account-wide usage.
    // Live plan-usage from the browser "Usage Bridge" extension (exact session +
    // weekly %, past Cloudflare). Ground truth for the Governor when present.
    let live_usage = usage_bridge::spawn(paths.ccwatch_dir(), || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    });
    {
        let shared = shared.clone();
        let cruise = cruise.clone();
        let config = Config::load(&paths.config_file());
        let paths2 = paths.clone();
        let remote_cache = remote_cache.clone();
        let remote_errors = manager.errors();
        let cruise_paths = paths.clone(); // for persisting the paced set
        let learned_path = paths.ccwatch_dir().join("learned.json");
        let mut learned = load_learned(&learned_path);
        let mut pacer_state = ccwatch_core::pacer::PacerState { price: 0.0 };
        let live_usage = live_usage.clone();
        let mut build = move |engine: &mut Engine| {
            let local = engine.refresh_now();
            let remotes = remote_cache.read().unwrap().clone();
            let mut snap = remote::merge(local, &remotes);
            for (name, err) in remote_errors.read().unwrap().iter() {
                snap.alerts.push(ccwatch_core::model::Alert {
                    severity: ccwatch_core::model::Severity::Warn,
                    kind: ccwatch_core::model::AlertKind::RemoteDown,
                    subject: name.clone(),
                    session_id: String::new(),
                    message: err.chars().take(120).collect(),
                    since_ms: snap.generated_at,
                });
            }

            // Governor: calibrate the ceiling. Confirmed walls (429+blackout)
            // SET the budget — up or down, so plan downgrades self-correct;
            // transient 429s and observed usage only raise it.
            let window_ms = config.governor_window_hours * 3_600_000;
            let updated = ccwatch_core::governor::learn(
                &snap.usage_buckets,
                &snap.rate_limits,
                window_ms,
                snap.generated_at,
                learned,
            );
            if updated != learned {
                learned = updated;
                if let Some(l) = &learned {
                    let _ = std::fs::write(
                        &learned_path,
                        serde_json::to_string(l).unwrap_or_default(),
                    );
                }
            }
            // Live extension reading wins over the (sporadic) transcript banner.
            let live = live_usage.read().ok().map(|u| *u).unwrap_or_default();
            let mut g = ccwatch_core::governor::compute(
                &snap.usage_buckets,
                &snap.rate_limits,
                snap.generated_at,
                &config,
                learned.map(|l| l.tokens),
                live.session.or(snap.window_usage_pct),
            );
            if let Some(alert) = ccwatch_core::governor::wall_alert(&g, snap.generated_at) {
                snap.alerts.push(alert);
            }
            // Weekly tank: one account-wide 7-day limit, anchored to Claude's
            // reported % when present, else limit markers + weighted buckets.
            g.week = ccwatch_core::governor::weekly_tank(
                &snap.usage_buckets,
                &snap.limit_hits,
                snap.generated_at,
                config.governor_week_budget,
                live.weekly.or(snap.weekly_usage_pct),
            );
            if let Some(alert) =
                ccwatch_core::governor::weekly_wall_alert(&g, snap.generated_at)
            {
                snap.alerts.push(alert);
            }
            snap.governor = Some(g);

            // Stamp the user's per-session priority overrides onto the snapshot
            // (keyed by stable session id) BEFORE the pacer reads them, so a pinned
            // High is protected and a pinned Low sheds first.
            {
                let c = cruise.lock().unwrap();
                if !c.overrides.is_empty() {
                    for s in &mut snap.sessions {
                        s.priority_override = c.overrides.get(&s.id).copied();
                    }
                }
            }

            // A fresh 429 within the last ~2 min is a hard AIMD signal; `rate_limits`
            // is the governor's own list of 429 epoch-ms timestamps, already in scope.
            let saw_429 = snap
                .rate_limits
                .iter()
                .any(|t| snap.generated_at - t < 120_000);
            let (plan, next_state) = ccwatch_core::pacer::plan(
                &snap,
                &config.pacer_config(),
                pacer_state,
                snap.generated_at,
                saw_429,
            );
            pacer_state = next_state;
            snap.pacing = Some(plan);

            // Autonomous enforcement — ONLY in "auto". In any non-auto mode the
            // per-tick loop takes NO pause/resume action, so a manual one-click
            // Apply stays paused until an explicit Release (`cruise_plan_actions`).
            {
                let mut c = cruise.lock().unwrap();
                let before = c.paced.clone();
                let plan_pause = snap
                    .pacing
                    .as_ref()
                    .map(|p| p.pause_targets())
                    .unwrap_or_default();
                let auto = c.mode == "auto";
                let (to_pause, to_resume) =
                    ccwatch_core::pacer::cruise_plan_actions(auto, &plan_pause, &c.paced);
                for t in to_resume {
                    // Untrack on a successful resume — a failed resume stays tracked
                    // so the next tick retries it (never leave a live session frozen
                    // and forgotten) — UNLESS the process is gone, in which case
                    // retrying forever would just fail every tick and stick the
                    // `paced` count on a dead pid: untrack those too.
                    if do_resume(&t) || !do_alive(&t) {
                        c.paced.remove(&t);
                    }
                }
                for t in to_pause {
                    if do_pause(&t) {
                        c.paced.insert(t);
                    }
                }
                // Prune targets whose LOCAL process has exited so the paced count
                // never sticks on a dead pid. Remote liveness isn't probed per tick
                // (that's a network round-trip); a remote target is cleared when the
                // plan stops naming it (auto) or on Release.
                c.paced
                    .retain(|t| t.ssh.is_some() || ccwatch_core::actions::alive(t.pid));
                if c.paced != before {
                    persist_paced(&cruise_paths, &c.paced);
                }
                // Mark exactly which sessions are frozen and sum their held burn,
                // so the UI can show what's paused (badge + numbers), not just a
                // bare count.
                let mut held = 0.0;
                for s in &mut snap.sessions {
                    let ssh = match &s.host {
                        ccwatch_core::model::Host::Remote { ssh_target, .. } => {
                            Some(ssh_target.clone())
                        }
                        _ => None,
                    };
                    let paused = s
                        .pid
                        .map(|pid| c.paced.contains(&PaceTarget { pid, ssh }))
                        .unwrap_or(false);
                    s.paused_by_cruise = paused;
                    if paused {
                        held += s.tokens_per_min;
                    }
                }
                if let Some(p) = snap.pacing.as_mut() {
                    p.auto = auto;
                    p.paced = c.paced.len();
                    p.paused_rate = held;
                }
                // "off" means Cruise is disabled: emit NO plan at all, so no client
                // shows an advisory, Apply button, or paced count. (Advisory shows
                // only in "advisory"/"auto"; enforcement runs only in "auto".)
                if c.mode == "off" {
                    snap.pacing = None;
                }
            }
            snap
        };
        let engine_config = Config::load(&paths.config_file());
        std::thread::spawn(move || {
            let mut engine = Engine::new(paths2, engine_config);
            // Prime immediately so early subscribers get real data.
            *shared.write().unwrap() = Arc::new(build(&mut engine));
            while tick_rx.recv().is_ok() {
                // Debounce: a burst of file events collapses into one refresh,
                // and refreshes are spaced at least 500 ms apart.
                std::thread::sleep(Duration::from_millis(150));
                while tick_rx.try_recv().is_ok() {}
                *shared.write().unwrap() = Arc::new(build(&mut engine));
                std::thread::sleep(Duration::from_millis(350));
            }
        });
    }

    // Accept connections.
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = shared.clone();
                let paths = paths.clone();
                let tick_tx = tick_tx.clone();
                let remote_defs = remote_defs.clone();
                let subscribers = subscribers.clone();
                let cruise = cruise.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(
                        stream, shared, paths, tick_tx, remote_defs, subscribers, cruise,
                    ) {
                        eprintln!("client error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

fn spawn_poll_timer(tx: Sender<()>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL_INTERVAL);
        if tx.send(()).is_err() {
            break;
        }
    });
}

/// Watch the Claude data dirs; coalesced events just send a tick.
fn spawn_fs_watcher(paths: &Paths, tx: Sender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(());
        }
    })
    .ok()?;
    // Best-effort: watch each dir that exists.
    for dir in [paths.sessions(), paths.tasks(), paths.projects()] {
        let _ = watcher.watch(&dir, RecursiveMode::Recursive);
    }
    Some(watcher)
}

fn handle_client(
    stream: UnixStream,
    shared: SharedSnapshot,
    paths: Paths,
    tick_tx: Sender<()>,
    remote_defs: Arc<Vec<RemoteDef>>,
    subscribers: Arc<AtomicUsize>,
    cruise: SharedCruise,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client hung up
    }
    let msg: ClientMsg = match serde_json::from_str(line.trim()) {
        Ok(m) => m,
        Err(e) => {
            send(&mut writer, &ServerMsg::ActionResult {
                ok: false,
                message: format!("bad request: {e}"),
            })?;
            return Ok(());
        }
    };

    match msg {
        ClientMsg::Subscribe => {
            subscribers.fetch_add(1, Ordering::Relaxed);
            let result = push_loop(&mut writer, &shared);
            subscribers.fetch_sub(1, Ordering::Relaxed);
            result?
        }
        ClientMsg::Action(req) => {
            let snapshot = shared.read().unwrap().clone();
            let outcome = execute_action(&req, &paths, &remote_defs, &snapshot, &cruise);
            let (ok, message) = match outcome {
                ActionOutcome::Ok(m) => (true, m),
                ActionOutcome::Failed(m) => (false, m),
            };
            log_action(&paths, &req, ok, &message);
            send(&mut writer, &ServerMsg::ActionResult { ok, message })?;
            let _ = tick_tx.send(()); // reflect the change quickly
        }
    }
    Ok(())
}

/// Push snapshots whenever they change, until the client disconnects (detected
/// on write error). A heartbeat goes out only after ~3s of silence, as a
/// liveness signal — not on every poll.
fn push_loop(writer: &mut UnixStream, shared: &SharedSnapshot) -> anyhow::Result<()> {
    let mut last_sent = 0i64;
    let mut last_write = std::time::Instant::now();
    loop {
        let snap = shared.read().unwrap().clone();
        if snap.generated_at != last_sent {
            last_sent = snap.generated_at;
            if send(writer, &ServerMsg::Snapshot(Box::new((*snap).clone()))).is_err() {
                break;
            }
            last_write = std::time::Instant::now();
        } else if last_write.elapsed() > Duration::from_secs(3) {
            if send(
                writer,
                &ServerMsg::Heartbeat {
                    at_ms: chrono::Utc::now().timestamp_millis(),
                },
            )
            .is_err()
            {
                break;
            }
            last_write = std::time::Instant::now();
        }
        std::thread::sleep(PUSH_INTERVAL);
    }
    Ok(())
}

fn execute_action(
    req: &ActionRequest,
    paths: &Paths,
    remote_defs: &[RemoteDef],
    snapshot: &Snapshot,
    cruise: &SharedCruise,
) -> ActionOutcome {
    match req {
        ActionRequest::KillSession { pid } => actions::terminate_session(*pid, KILL_GRACE),
        ActionRequest::PauseSession { pid } => actions::pause(*pid),
        ActionRequest::ResumeSession { pid } => actions::resume(*pid),
        ActionRequest::KillBackground { pid } => actions::kill_background(*pid),
        ActionRequest::DisableHook {
            settings_path,
            event,
            command,
        } => actions::disable_hook(std::path::Path::new(settings_path), event, command),
        ActionRequest::CancelRemote { remote, id } => {
            cancel_remote(remote_defs, remote, id, snapshot)
        }
        ActionRequest::ApplyPacing => {
            let targets = snapshot
                .pacing
                .as_ref()
                .map(|p| p.pause_targets())
                .unwrap_or_default();
            let mut paused = 0usize;
            let mut c = cruise.lock().unwrap();
            for t in targets {
                if do_pause(&t) {
                    paused += 1;
                    // Register the target as Cruise-paced so it's releasable via
                    // SetCruiseMode/Release, survives a daemon restart via the
                    // startup sweep, and — because the non-auto refresher no longer
                    // resumes — STAYS paused until the user releases it.
                    c.paced.insert(t);
                }
            }
            persist_paced(paths, &c.paced);
            ActionOutcome::Ok(format!("Cruise: paused {paused} background session(s)"))
        }
        ActionRequest::SetCruiseMode { mode } => {
            // Normalize so "Auto"/" auto " engage auto instead of silently
            // acting as off.
            let mode = mode.trim().to_lowercase();
            let mut c = cruise.lock().unwrap();
            c.mode = mode.clone();
            let mut released = 0usize;
            if mode != "auto" {
                // Release: resume everything Cruise paused. Best-effort — the
                // non-auto refresher no longer auto-resumes (so manual Applies are
                // sticky), so this is the release path. A resume that fails on a
                // still-live process leaves it tracked (retried on the next Release
                // or when auto re-engages); a dead process is untracked here so the
                // count can't stick on it.
                let targets: Vec<PaceTarget> = c.paced.iter().cloned().collect();
                for t in targets {
                    if do_resume(&t) || !do_alive(&t) {
                        c.paced.remove(&t);
                        released += 1;
                    }
                }
            }
            persist_paced(paths, &c.paced);
            ActionOutcome::Ok(format!("Cruise mode = {mode}; released {released}"))
        }
        ActionRequest::SetSessionPriority { session_id, priority } => {
            let mut c = cruise.lock().unwrap();
            let tier = match priority.trim().to_lowercase().as_str() {
                "high" => Some(Priority::High),
                "low" => Some(Priority::Low),
                _ => None, // "normal"/anything else clears the override
            };
            match tier {
                Some(p) => {
                    c.overrides.insert(session_id.clone(), p);
                }
                None => {
                    c.overrides.remove(session_id);
                }
            }
            persist_overrides(paths, &c.overrides);
            ActionOutcome::Ok(format!("Cruise priority: {session_id} = {priority}"))
        }
    }
}

/// Cancel an entity on a remote host. An explicitly configured cancel command
/// wins; for SSH hosts without one, the zero-config default is to TERM the
/// session's pid on the remote — resolved from the latest merged snapshot.
fn cancel_remote(defs: &[RemoteDef], remote: &str, id: &str, snap: &Snapshot) -> ActionOutcome {
    use ccwatch_core::remote::CommandRunner;
    let Some(def) = defs.iter().find(|d| d.name == remote) else {
        return ActionOutcome::Failed(format!("unknown remote '{remote}'"));
    };

    let argv = match def.cancel_argv(id) {
        Some(argv) => argv,
        None => {
            // SSH default: kill the remote pid.
            let Some(target) = &def.target else {
                return ActionOutcome::Failed(format!("remote '{remote}' has no cancel command"));
            };
            let Some(pid) = snap
                .sessions
                .iter()
                .find(|s| s.id == id && s.remote_name.as_deref() == Some(remote))
                .and_then(|s| s.pid)
            else {
                return ActionOutcome::Failed(format!(
                    "session {id} on '{remote}' has no known pid to kill"
                ));
            };
            vec![
                "ssh".into(),
                "-T".into(),
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=5".into(),
                target.clone(),
                "kill".into(),
                "-TERM".into(),
                pid.to_string(),
            ]
        }
    };

    match SystemRunner.run(&argv, None, KILL_GRACE.saturating_mul(5)) {
        Ok(out) => ActionOutcome::Ok(format!("cancelled {id} on {remote} {}", out.trim())),
        Err(e) => ActionOutcome::Failed(format!("cancel {id} on {remote} failed: {e}")),
    }
}

fn send(writer: &mut UnixStream, msg: &ServerMsg) -> std::io::Result<()> {
    let mut line = serde_json::to_string(msg).unwrap_or_else(|_| "{}".into());
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// Read the persisted learned budget. Understands both the current
/// `LearnedBudget` format and the legacy `{"window_budget_learned":N}`
/// (migrated as soft evidence so a confirmed wall can lower it).
fn load_learned(path: &std::path::Path) -> Option<ccwatch_core::governor::LearnedBudget> {
    let text = std::fs::read_to_string(path).ok()?;
    if let Ok(l) = serde_json::from_str::<ccwatch_core::governor::LearnedBudget>(&text) {
        return Some(l);
    }
    let legacy = serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("window_budget_learned")?
        .as_u64()?;
    Some(ccwatch_core::governor::LearnedBudget {
        tokens: legacy,
        hard: false,
        at_ms: 0,
    })
}



fn read_pidfile(paths: &Paths) -> Option<i32> {
    std::fs::read_to_string(paths.pidfile())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn log_action(paths: &Paths, req: &ActionRequest, ok: bool, message: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.action_log())
    {
        let ts = chrono::Utc::now().to_rfc3339();
        let _ = writeln!(
            f,
            "{ts}\t{}\tok={ok}\t{message}",
            serde_json::to_string(req).unwrap_or_default()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::load_learned;

    #[test]
    fn load_learned_reads_both_formats() {
        let dir = std::env::temp_dir().join(format!("ccw-learn-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("learned.json");

        std::fs::write(&p, r#"{"window_budget_learned":23713076}"#).unwrap();
        let l = load_learned(&p).unwrap();
        assert_eq!(l.tokens, 23_713_076);
        assert!(!l.hard, "legacy value is soft so a wall can lower it");

        std::fs::write(&p, r#"{"tokens":12000000,"hard":true,"at_ms":5}"#).unwrap();
        let l = load_learned(&p).unwrap();
        assert_eq!((l.tokens, l.hard, l.at_ms), (12_000_000, true, 5));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod pacing_tests {
    use ccwatch_core::model::Snapshot;
    use ccwatch_core::pacer::{PacerConfig, PacerState};

    #[test]
    fn plan_attached_when_governor_present_is_computed() {
        // A snapshot with no governor yields a None-ish plan but must not panic,
        // and state price is carried through.
        let snap = Snapshot::empty(1_000);
        let (planr, st) = ccwatch_core::pacer::plan(
            &snap, &PacerConfig::default(), PacerState { price: 0.5 }, 1_000, false,
        );
        assert_eq!(st.price, 0.5, "price carried through when no governor");
        assert!(planr.actions.is_empty());
    }
}

#[cfg(test)]
mod cruise_tests {
    use super::{cruise_paced_file, persist_paced, sweep_orphaned_paced};
    use ccwatch_core::model::PaceTarget;
    use ccwatch_core::pacer::cruise_plan_actions;
    use ccwatch_core::Paths;
    use std::collections::HashSet;

    fn pt(pid: i32) -> PaceTarget {
        PaceTarget { pid, ssh: None }
    }

    #[test]
    fn non_auto_keeps_manual_pauses_until_release() {
        // Sticky Apply: in a non-auto mode the per-tick loop resumes nothing, so a
        // manual one-click pause survives until an explicit Release. (Regression
        // guard: previously non-auto was modeled as "plan wants nothing" and the
        // refresher resumed every paced pid within a tick, undoing Apply.)
        let paced: HashSet<PaceTarget> = [pt(11), pt(12), pt(13)].into_iter().collect();
        let (to_pause, to_resume) = cruise_plan_actions(false, &[], &paced);
        assert!(to_pause.is_empty());
        assert!(to_resume.is_empty(), "non-auto must not auto-resume manual pauses");
    }

    #[test]
    fn paced_file_round_trips_the_set() {
        let root = std::env::temp_dir().join(format!("ccw-paced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::new(&root);
        std::fs::create_dir_all(paths.ccwatch_dir()).unwrap();

        // A mix of local and remote targets must round-trip intact.
        let paced: HashSet<PaceTarget> = [
            pt(101),
            pt(202),
            PaceTarget { pid: 303, ssh: Some("u@h".into()) },
        ]
        .into_iter()
        .collect();
        persist_paced(&paths, &paced);

        let text = std::fs::read_to_string(cruise_paced_file(&paths)).unwrap();
        let back: HashSet<PaceTarget> = serde_json::from_str::<Vec<PaceTarget>>(&text)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(back, paced, "persisted paced set must round-trip (incl. ssh host)");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_is_a_noop_when_file_absent_and_clears_after() {
        let root = std::env::temp_dir().join(format!("ccw-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths::new(&root);
        std::fs::create_dir_all(paths.ccwatch_dir()).unwrap();

        // No file → nothing to sweep, no crash, no file created.
        assert_eq!(sweep_orphaned_paced(&paths), 0);
        assert!(!cruise_paced_file(&paths).exists());

        // With dead/never-existed pids, resume() fails so none are counted, and
        // the file is cleared to an empty array regardless.
        let bogus: HashSet<PaceTarget> =
            [pt(2_000_000_001), pt(2_000_000_002)].into_iter().collect();
        persist_paced(&paths, &bogus);
        let _ = sweep_orphaned_paced(&paths); // resume of bogus pids fails → 0-ish
        let after = std::fs::read_to_string(cruise_paced_file(&paths)).unwrap();
        assert_eq!(after.trim(), "[]", "sweep must clear the file");

        let _ = std::fs::remove_dir_all(&root);
    }
}
