//! Pure snapshot → menu-bar presentation logic. Kept free of any GUI toolkit so
//! it's unit-testable; `main.rs` binds this model to `tray-icon`/`muda` items.

use ccwatch_core::model::{Host, SessionState, Snapshot};

/// Compact rate: 1234 → "1k", 3_400_000 → "3.4M".
pub fn rate(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.0}k", v / 1_000.0)
    } else {
        format!("{v:.0}")
    }
}

fn tokens(n: u64) -> String {
    rate(n as f64)
}

fn host_label(h: &Host) -> String {
    match h {
        Host::Local => "local".into(),
        Host::Remote { name, .. } => name.clone(),
        Host::Cloud => "cloud".into(),
    }
}

/// Format a cruise delta: `▲2.1×` above cruise, `▼0.6×` below, `⛔` when the
/// tank is empty and still burning.
pub fn delta_str(delta: f64) -> String {
    if delta.is_infinite() || delta >= ccwatch_core::governor::DELTA_EMPTY {
        "⛔".to_string()
    } else if delta >= 1.0 {
        format!("▲{delta:.1}×")
    } else {
        format!("▼{delta:.1}×")
    }
}

fn clock(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%H:%M").to_string(),
        None => "--:--".into(),
    }
}

fn mins(m: f64) -> String {
    if m >= 60.0 {
        format!("{}h{:02}m", (m as i64) / 60, (m as i64) % 60)
    } else {
        format!("{m:.0}m")
    }
}

/// Menu-bar text next to the graph. Alerts dominate; then the governor's
/// cruise delta (the throttle readout); then the raw burn rate. `⏻` when the
/// daemon is unreachable.
pub fn tray_title(s: &Snapshot, connected: bool, mode: crate::prefs::TitleMode) -> String {
    use crate::prefs::TitleMode as M;
    if !connected {
        return "⏻".to_string();
    }
    let main = match mode {
        M::Throttle => s
            .governor
            .as_ref()
            .and_then(|g| g.primary_delta())
            .map(delta_str)
            .unwrap_or_else(|| rate(s.totals.tokens_per_min)),
        M::Rate => rate(s.totals.tokens_per_min),
        M::Range => s
            .governor
            .as_ref()
            .and_then(|g| g.window.range_min)
            .map(mins)
            .unwrap_or_else(|| "—".into()),
        M::Tank => s
            .governor
            .as_ref()
            .and_then(|g| {
                let w = &g.window;
                w.budget
                    .map(|b| format!("{:.0}%", 100.0 - (w.used as f64 / b as f64 * 100.0).min(100.0)))
            })
            .unwrap_or_else(|| "—".into()),
        M::Nothing => String::new(),
    };
    // Alerts always stay visible next to whatever the mode shows.
    match (main.is_empty(), s.alerts.len()) {
        (true, 0) => String::new(),
        (true, n) => format!("⚠{n}"),
        (false, 0) => main,
        (false, n) => format!("{main} ⚠{n}"),
    }
}

/// The governor line for the dropdown: throttle, range, tank, reset.
pub fn governor_line(s: &Snapshot) -> String {
    let Some(g) = &s.governor else {
        return "governor: no data".into();
    };
    let w = &g.window;
    let mut parts = Vec::new();
    match w.delta {
        Some(d) => parts.push(format!("throttle {}", delta_str(d))),
        None => parts.push(format!("burn {}/min", rate(w.rate_per_min))),
    }
    if let Some(r) = w.range_min {
        parts.push(format!("range {}", mins(r)));
    }
    if let (Some(b), used) = (w.budget, w.used) {
        let pct = 100.0 - (used as f64 / b as f64 * 100.0).min(100.0);
        let tag = if w.budget_source == ccwatch_core::model::BudgetSource::Learned {
            "~"
        } else {
            ""
        };
        parts.push(format!("tank {tag}{pct:.0}%"));
    } else {
        parts.push(format!("used {}", tokens(w.used)));
    }
    if let Some(reset) = w.resets_at {
        parts.push(format!("reset {}", clock(reset)));
    }
    if let (Some(b), Some(d)) = (g.cruise.budget, g.cruise.delta) {
        parts.push(format!(
            "hour {}/{} {}",
            tokens(g.cruise.used),
            tokens(b),
            delta_str(d)
        ));
    }
    parts.join(" · ")
}

pub fn tooltip(s: &Snapshot) -> String {
    format!(
        "ccwatch — {} active · {} tok/min · {} alerts",
        s.totals.active_sessions,
        rate(s.totals.tokens_per_min),
        s.alerts.len()
    )
}

/// What `main.rs` can do when a session's action item is clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionAction {
    /// Local session: signal this pid.
    Signal { pid: i32 },
    /// Remote/cloud session: run the host's cancel command for this id.
    Cancel { remote: String, id: String },
    /// Nothing possible (no pid / no cancel configured).
    None,
}

/// One session's dropdown entry: a submenu title, info lines, and its action.
#[derive(Clone, Debug)]
pub struct SessionEntry {
    /// Session id — keys the per-session sparkline history.
    pub id: String,
    pub title: String,
    /// Text shown beside the sparkline (token breakdown).
    pub tokens_line: String,
    /// Informational lines shown inside the submenu.
    pub info: Vec<String>,
    pub name: String,
    pub action: SessionAction,
    /// Label for the destructive action ("Kill session…" / "Cancel on demo-host…").
    pub kill_label: String,
    /// Pause/resume only make sense for local pids.
    pub can_pause: bool,
    /// Current burn rate, fed into the sparkline history.
    pub tokens_per_min: f64,
}

/// The whole dropdown, as pure data.
#[derive(Clone, Debug)]
pub struct MenuModel {
    pub header: String,
    pub alerts: Vec<String>,
    pub sessions: Vec<SessionEntry>,
}

/// "local 5" or "local 5 · demo-host 2 · cloud 1" — hosts in stable order.
fn host_counts(s: &Snapshot) -> String {
    let mut counts: std::collections::BTreeMap<(u8, String), usize> = Default::default();
    for sess in &s.sessions {
        let rank = match sess.host {
            Host::Local => 0u8,
            Host::Remote { .. } => 1,
            Host::Cloud => 2,
        };
        *counts.entry((rank, host_label(&sess.host))).or_default() += 1;
    }
    if counts.is_empty() {
        return "0 active".into();
    }
    counts
        .into_iter()
        .map(|((_, label), n)| format!("{label} {n}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

pub fn menu_model(s: &Snapshot) -> MenuModel {
    let header = format!(
        "{} · {} tok/min · Σ {} · cache {:.0}%",
        host_counts(s),
        rate(s.totals.tokens_per_min),
        tokens(s.totals.total_tokens),
        s.totals.cache_hit_pct
    );

    let alerts = s
        .alerts
        .iter()
        .map(|a| format!("⚠ {} — {}: {}", a.kind.label(), a.subject, a.message))
        .collect();

    let sessions = s
        .sessions
        .iter()
        .map(|sess| {
            let host = match &sess.host {
                Host::Local => String::new(),
                other => format!("  ·  {}", host_label(other)),
            };
            let state = match sess.state {
                SessionState::Running => "running",
                SessionState::Waiting => "waiting",
                SessionState::Idle => "idle",
                SessionState::Ended => "ended",
            };
            let t = &sess.tokens;
            let tokens_line = format!(
                "in {} · out {} · cw {} · cr {} · {} msgs",
                tokens(t.input),
                tokens(t.output),
                tokens(t.cache_write),
                tokens(t.cache_read),
                t.messages
            );
            let mut work: Vec<String> = sess
                .activity
                .iter()
                .take(2)
                .map(|a| {
                    let tail: String = if a.detail.chars().count() > 28 {
                        format!("…{}", a.detail.chars().skip(a.detail.chars().count() - 27).collect::<String>())
                    } else {
                        a.detail.clone()
                    };
                    format!("{} {}", a.tool, tail)
                })
                .collect();
            work.extend(
                sess.processes
                    .iter()
                    .take(2)
                    .map(|p| format!("{} {:.0}%", p.name, p.cpu_pct)),
            );
            let procs = if work.is_empty() {
                "no live activity".to_string()
            } else {
                format!("now · {}", work.join(" · "))
            };
            let info = vec![
                format!(
                    "{state} · {} · cpu {:.0}% · {} MB",
                    sess.model.as_deref().unwrap_or("-").trim_start_matches("claude-"),
                    sess.cpu_pct,
                    sess.rss_mb
                ),
                format!("cwd {}", sess.cwd),
                procs,
            ];
            let local = matches!(sess.host, Host::Local);
            let (action, kill_label) = if local {
                match sess.pid {
                    Some(pid) => (SessionAction::Signal { pid }, "Kill session…".to_string()),
                    None => (SessionAction::None, "No pid — cannot signal".to_string()),
                }
            } else {
                match &sess.remote_name {
                    Some(remote) => (
                        SessionAction::Cancel {
                            remote: remote.clone(),
                            id: sess.id.clone(),
                        },
                        format!("Cancel on {}…", host_label(&sess.host)),
                    ),
                    None => (SessionAction::None, "No cancel command configured".to_string()),
                }
            };
            let display = sess.title.as_deref().unwrap_or(&sess.name);
            SessionEntry {
                id: sess.id.clone(),
                title: format!("{display}  —  {} tok/min{host}", rate(sess.tokens_per_min)),
                tokens_line,
                info,
                name: sess.name.clone(),
                action,
                kill_label,
                can_pause: local && sess.pid.is_some(),
                tokens_per_min: sess.tokens_per_min,
            }
        })
        .collect();

    MenuModel {
        header,
        alerts,
        sessions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccwatch_core::model::*;

    fn base() -> Snapshot {
        let mut s = Snapshot::empty(0);
        s.totals = Totals {
            active_sessions: 2,
            tokens_per_min: 46_000.0,
            total_tokens: 1_000_000,
            cache_hit_pct: 91.0,
        };
        s
    }

    fn sess(name: &str, tpm: f64, host: Host) -> Session {
        Session {
            id: format!("id-{name}"),
            name: name.into(),
            title: None,
            cwd: "/x".into(),
            pid: Some(4242),
            kind: "interactive".into(),
            entrypoint: "cli".into(),
            version: "1".into(),
            model: Some("claude-opus-4-8".into()),
            state: SessionState::Running,
            started_at: Some(0),
            last_activity: Some(0),
            tokens: TokenLedger::default(),
            tokens_per_min: tpm,
            cpu_pct: 12.0,
            rss_mb: 200,
            agents: vec![],
            tasks: vec![],
            watchers: vec![],
            activity: vec![],
            processes: vec![],
            host,
            remote_name: None,
        }
    }

    #[test]
    fn title_alerts_rate_and_disconnected() {
        let mut s = base();
        assert_eq!(tray_title(&s, true, crate::prefs::TitleMode::Throttle), "46k");
        assert_eq!(tray_title(&s, false, crate::prefs::TitleMode::Throttle), "⏻");
        s.alerts.push(Alert {
            severity: Severity::Critical,
            kind: AlertKind::RunawayLoop,
            subject: "webapp".into(),
            session_id: "b".into(),
            message: "burning".into(),
            since_ms: 0,
        });
        // No governor data → alert count next to the rate fallback.
        assert_eq!(tray_title(&s, true, crate::prefs::TitleMode::Throttle), "46k ⚠1");
    }

    #[test]
    fn local_session_gets_signal_action_and_pause() {
        let mut s = base();
        s.sessions.push(sess("webapp", 40_000.0, Host::Local));
        let m = menu_model(&s);
        assert_eq!(m.sessions.len(), 1);
        let e = &m.sessions[0];
        assert_eq!(e.action, SessionAction::Signal { pid: 4242 });
        assert!(e.can_pause);
        assert_eq!(e.kill_label, "Kill session…");
        assert!(e.title.contains("webapp"));
        assert!(e.title.contains("40k tok/min"));
        assert_eq!(e.id, "id-webapp");
        assert_eq!(e.tokens_per_min, 40_000.0);
        assert!(e.tokens_line.contains("in ") && e.tokens_line.contains("msgs"));
        assert_eq!(e.info.len(), 3);
        assert!(e.info[2].contains("no live activity"));
        assert!(e.info[0].contains("running"));
        assert!(e.info[0].contains("opus-4-8"));
        assert!(e.info[1].contains("cwd /x"));
    }

    #[test]
    fn remote_session_gets_cancel_action_no_pause() {
        let mut s = base();
        let mut r = sess(
            "worker",
            6_000.0,
            Host::Remote {
                name: "demo-host".into(),
                ssh_target: "demo-host".into(),
            },
        );
        r.remote_name = Some("demo-host".into());
        s.sessions.push(r);
        let m = menu_model(&s);
        let e = &m.sessions[0];
        assert_eq!(
            e.action,
            SessionAction::Cancel {
                remote: "demo-host".into(),
                id: "id-worker".into()
            }
        );
        assert!(!e.can_pause);
        assert_eq!(e.kill_label, "Cancel on demo-host…");
        assert!(e.title.contains("demo-host"));
    }

    #[test]
    fn remote_without_cancel_config_is_inert() {
        let mut s = base();
        s.sessions.push(sess("cloudy", 1_000.0, Host::Cloud));
        let m = menu_model(&s);
        assert_eq!(m.sessions[0].action, SessionAction::None);
        assert!(!m.sessions[0].can_pause);
    }

    #[test]
    fn header_and_alert_lines() {
        let mut s = base();
        s.alerts.push(Alert {
            severity: Severity::Warn,
            kind: AlertKind::CacheBleed,
            subject: "worker".into(),
            session_id: "w".into(),
            message: "cache-read 12%".into(),
            since_ms: 0,
        });
        s.sessions.push(sess("a", 1.0, Host::Local));
        s.sessions.push(sess("b", 1.0, Host::Local));
        let mut r = sess(
            "c",
            1.0,
            Host::Remote {
                name: "demo-host".into(),
                ssh_target: "demo-host".into(),
            },
        );
        r.remote_name = Some("demo-host".into());
        s.sessions.push(r);
        let m = menu_model(&s);
        // Per-host counts, local first.
        assert!(m.header.starts_with("local 2 · demo-host 1"), "header: {}", m.header);
        assert!(m.header.contains("46k tok/min"));
        assert_eq!(m.alerts.len(), 1);
        assert!(m.alerts[0].contains("cache bleed"));
        assert!(m.alerts[0].contains("worker"));
    }

    #[test]
    fn tray_title_prefers_governor_delta() {
        let mut s = base();
        let tank = Tank {
            used: 500_000,
            budget: Some(1_000_000),
            budget_source: BudgetSource::Config,
            window_start: 0,
            resets_at: Some(10_800_000),
            rate_per_min: 10_000.0,
            cruise_per_min: Some(2_778.0),
            delta: Some(3.6),
            range_min: Some(50.0),
            wall_at: Some(3_000_000),
        };
        s.governor = Some(GovernorStatus { window: tank, cruise: tank });
        use crate::prefs::TitleMode as M;
        assert_eq!(tray_title(&s, true, M::Throttle), "▲3.6×");
        assert_eq!(tray_title(&s, true, M::Rate), "46k");
        assert_eq!(tray_title(&s, true, M::Range), "50m");
        assert_eq!(tray_title(&s, true, M::Tank), "50%");
        assert_eq!(tray_title(&s, true, M::Nothing), "");
        // Alerts still win.
        s.alerts.push(Alert {
            severity: Severity::Critical,
            kind: AlertKind::BudgetWall,
            subject: "plan window".into(),
            session_id: String::new(),
            message: "wall".into(),
            since_ms: 0,
        });
        assert_eq!(tray_title(&s, true, M::Throttle), "▲3.6× ⚠1", "delta stays visible with alerts");
        assert_eq!(tray_title(&s, true, M::Nothing), "⚠1", "alerts still surface in graph-only mode");
    }

    #[test]
    fn governor_line_reads_like_a_gauge() {
        let mut s = base();
        let mut tank = Tank {
            used: 620_000,
            budget: Some(1_000_000),
            budget_source: BudgetSource::Learned,
            window_start: 0,
            resets_at: Some(1_000_000_000),
            rate_per_min: 10_000.0,
            cruise_per_min: Some(5_000.0),
            delta: Some(2.0),
            range_min: Some(98.0),
            wall_at: Some(999_999),
        };
        let cruise = {
            tank.budget = Some(300_000);
            tank.used = 150_000;
            tank.delta = Some(0.5);
            tank
        };
        let mut window = tank;
        window.budget = Some(1_000_000);
        window.used = 620_000;
        window.delta = Some(2.0);
        s.governor = Some(GovernorStatus { window, cruise });
        let line = governor_line(&s);
        assert!(line.contains("throttle ▲2.0×"), "{line}");
        assert!(line.contains("range 1h38m"), "{line}");
        assert!(line.contains("tank ~38%"), "learned budgets marked ~: {line}");
        assert!(line.contains("hour 150k/300k ▼0.5×"), "{line}");
    }

    #[test]
    fn delta_formats() {
        assert_eq!(delta_str(2.14), "▲2.1×");
        assert_eq!(delta_str(0.6), "▼0.6×");
        assert_eq!(delta_str(f64::INFINITY), "⛔");
        assert_eq!(delta_str(99.0), "⛔");
    }

    #[test]
    fn remote_down_alert_renders() {
        let mut s = base();
        s.alerts.push(Alert {
            severity: Severity::Warn,
            kind: AlertKind::RemoteDown,
            subject: "demo-host".into(),
            session_id: String::new(),
            message: "ssh: connect timed out".into(),
            since_ms: 0,
        });
        let m = menu_model(&s);
        assert!(m.alerts[0].contains("remote down"));
        assert!(m.alerts[0].contains("demo-host"));
    }
}
