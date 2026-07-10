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
use ccwatch_core::model::Snapshot;
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
    if !std::env::args().any(|a| a == "--persist") {
        let grace = std::env::var("CCWATCH_IDLE_EXIT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(IDLE_EXIT);
        let subs = subscribers.clone();
        let sock = sock.clone();
        let pidfile = paths.pidfile();
        std::thread::spawn(move || {
            let mut last_active = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                if subs.load(Ordering::Relaxed) > 0 {
                    last_active = Instant::now();
                } else if last_active.elapsed() > grace {
                    eprintln!("no clients for {grace:?}; exiting");
                    let _ = std::fs::remove_file(&sock);
                    let _ = std::fs::remove_file(&pidfile);
                    std::process::exit(0);
                }
            }
        });
    }

    // Shared latest snapshot, updated by the refresher thread.
    let shared: SharedSnapshot = Arc::new(RwLock::new(Arc::new(Snapshot::empty(0))));

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
        let config = Config::load(&paths.config_file());
        let paths2 = paths.clone();
        let remote_cache = remote_cache.clone();
        let remote_errors = manager.errors();
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

            let saw_429 = false; // Step 1: no 429 threading yet; Autonomous step wires this.
            let (plan, next_state) = ccwatch_core::pacer::plan(
                &snap,
                &config.pacer_config(),
                pacer_state,
                snap.generated_at,
                saw_429,
            );
            pacer_state = next_state;
            snap.pacing = Some(plan);
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
                std::thread::spawn(move || {
                    if let Err(e) =
                        handle_client(stream, shared, paths, tick_tx, remote_defs, subscribers)
                    {
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
            let outcome = execute_action(&req, &paths, &remote_defs, &snapshot);
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
    _paths: &Paths,
    remote_defs: &[RemoteDef],
    snapshot: &Snapshot,
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
            let pids = snapshot
                .pacing
                .as_ref()
                .map(|p| p.pause_pids())
                .unwrap_or_default();
            let mut paused = 0usize;
            for pid in pids {
                if matches!(actions::pause(pid), ActionOutcome::Ok(_)) {
                    paused += 1;
                }
            }
            ActionOutcome::Ok(format!("Cruise: paused {paused} background session(s)"))
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
