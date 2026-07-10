//! Phase 2 — remote hosts (SSH machines and cloud agents/routines).
//!
//! The unifying idea: **a remote source is any command whose stdout is a
//! [`Snapshot`] JSON** (the same schema `ccwatchd --once` prints). An SSH host
//! is `ssh <target> ccwatchd --once`; a cloud source is a user script that
//! queries the cloud API and emits the same shape. This reuses the whole engine
//! and keeps `core` dependency-free — the only new capability is "run a command
//! and read its stdout", abstracted behind [`CommandRunner`] so it's mockable.

use crate::model::{Host, Snapshot};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The zero-install remote probe: piped over `ssh <host> python3 -`, it reads
/// the remote's `~/.claude` and prints a [`Snapshot`] JSON. Nothing has to be
/// installed on the remote machine.
pub const PROBE_PY: &str = include_str!("probe.py");

/// Runs an argv (optionally feeding `stdin`) and returns its stdout.
/// Injectable for tests.
pub trait CommandRunner: Send + Sync {
    fn run(&self, argv: &[String], stdin: Option<&str>, timeout: Duration)
        -> anyhow::Result<String>;
}

/// The real runner: spawns the process, feeds stdin and reads stdout on
/// threads (so large payloads can't deadlock on a full pipe), and enforces a
/// timeout.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        argv: &[String],
        stdin: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        if argv.is_empty() {
            anyhow::bail!("empty command");
        }
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {:?}: {e}", argv[0]))?;

        if let Some(payload) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            let payload = payload.to_string();
            // Write on a thread and drop the handle so the child sees EOF.
            std::thread::spawn(move || {
                let _ = pipe.write_all(payload.as_bytes());
            });
        }

        let mut stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            let _ = tx.send(s);
        });

        let start = Instant::now();
        loop {
            match child.try_wait()? {
                Some(status) => {
                    let out = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
                    if !status.success() {
                        anyhow::bail!("command exited with {status}");
                    }
                    return Ok(out);
                }
                None => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        anyhow::bail!("command timed out after {:?}", timeout);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RemoteKind {
    #[default]
    Ssh,
    Cloud,
}

/// One configured remote source, from `~/.claude/ccwatch/remotes.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct RemoteDef {
    pub name: String,
    #[serde(default)]
    pub kind: RemoteKind,
    /// SSH target (`user@host`) — used to build the default fetch command.
    #[serde(default)]
    pub target: Option<String>,
    /// Explicit fetch argv. If empty, defaults to `ssh <target> ccwatchd --once`.
    #[serde(default)]
    pub fetch: Vec<String>,
    /// Cancel argv template; the literal token `{id}` is replaced with the
    /// entity id. Empty means "cancel not supported".
    #[serde(default)]
    pub cancel: Vec<String>,
}

impl RemoteDef {
    /// How to fetch this remote's snapshot: `(argv, stdin payload)`.
    ///
    /// An explicit `fetch` wins. Otherwise SSH hosts get the **zero-install
    /// probe**: `ssh <target> python3 -` with [`PROBE_PY`] on stdin — nothing
    /// needs to be installed on the remote. BatchMode keeps an unreachable or
    /// password-prompting host from hanging the fetch loop.
    pub fn fetch_plan(&self) -> Option<(Vec<String>, Option<&'static str>)> {
        if !self.fetch.is_empty() {
            return Some((self.fetch.clone(), None));
        }
        match (&self.kind, &self.target) {
            (RemoteKind::Ssh, Some(t)) => Some((
                vec![
                    "ssh".into(),
                    "-T".into(),
                    "-o".into(),
                    "BatchMode=yes".into(),
                    "-o".into(),
                    "ConnectTimeout=5".into(),
                    t.clone(),
                    "python3".into(),
                    "-".into(),
                ],
                Some(PROBE_PY),
            )),
            _ => None,
        }
    }

    /// This remote's [`Host`] tag.
    pub fn host(&self) -> Host {
        match self.kind {
            RemoteKind::Ssh => Host::Remote {
                name: self.name.clone(),
                ssh_target: self.target.clone().unwrap_or_default(),
            },
            RemoteKind::Cloud => Host::Cloud,
        }
    }

    /// Build the cancel argv for `id`, or `None` if unsupported.
    pub fn cancel_argv(&self, id: &str) -> Option<Vec<String>> {
        if self.cancel.is_empty() {
            return None;
        }
        Some(
            self.cancel
                .iter()
                .map(|part| part.replace("{id}", id))
                .collect(),
        )
    }
}

/// Fetch one remote's snapshot and retag its sessions/alerts with this host.
pub fn fetch_remote(
    def: &RemoteDef,
    runner: &dyn CommandRunner,
    timeout: Duration,
) -> anyhow::Result<Snapshot> {
    let Some((argv, stdin)) = def.fetch_plan() else {
        anyhow::bail!("remote '{}' has no fetch command", def.name);
    };
    let out = runner.run(&argv, stdin, timeout)?;
    let mut snap: Snapshot = serde_json::from_str(out.trim())
        .map_err(|e| anyhow::anyhow!("remote '{}' returned invalid snapshot: {e}", def.name))?;
    let host = def.host();
    for s in &mut snap.sessions {
        s.host = host.clone();
        s.remote_name = Some(def.name.clone());
    }
    Ok(snap)
}

/// Merge a local snapshot with zero or more remote snapshots into one aggregate,
/// recomputing totals across every host.
pub fn merge(local: Snapshot, remotes: &[Snapshot]) -> Snapshot {
    // Capture Copy fields before `local` is partially moved below.
    let local_weekly = local.weekly_usage_pct;
    let local_window = local.window_usage_pct;
    let mut sessions = local.sessions;
    let mut alerts = local.alerts;
    for r in remotes {
        sessions.extend(r.sessions.iter().cloned());
        alerts.extend(r.alerts.iter().cloned());
    }
    // Sort sessions so same-host rows are contiguous, hottest first within host.
    sessions.sort_by(|a, b| {
        host_rank(&a.host)
            .cmp(&host_rank(&b.host))
            .then(a.host.label().cmp(&b.host.label()))
            .then(
                b.tokens_per_min
                    .partial_cmp(&a.tokens_per_min)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    alerts.sort_by_key(|a| std::cmp::Reverse(a.severity));

    let totals = crate::model::Totals {
        active_sessions: sessions.len(),
        tokens_per_min: sessions.iter().map(|s| s.tokens_per_min).sum(),
        total_tokens: sessions.iter().map(|s| s.tokens.grand_total()).sum(),
        cache_hit_pct: {
            let mut agg = crate::model::TokenLedger::default();
            for s in &sessions {
                agg.add(&s.tokens);
            }
            agg.cache_hit_ratio().unwrap_or(0.0) * 100.0
        },
    };

    // Combine usage buckets across hosts — limits are per account, so remote
    // burn draws from the same tank.
    let bucket_lists: Vec<&[(i64, u64)]> = std::iter::once(&local.usage_buckets[..])
        .chain(remotes.iter().map(|r| &r.usage_buckets[..]))
        .collect();
    let usage_buckets = crate::governor::merge_buckets(&bucket_lists);
    let mut rate_limits: Vec<i64> = local.rate_limits.clone();
    for r in remotes {
        rate_limits.extend(&r.rate_limits);
    }
    rate_limits.sort_unstable();
    rate_limits.dedup();
    // Weekly cap is per-account: limit markers combine across every host too.
    let mut limit_hits = local.limit_hits.clone();
    for r in remotes {
        limit_hits.extend(&r.limit_hits);
    }
    limit_hits.sort_by_key(|h| h.at_ms);
    limit_hits.dedup();

    // Model mix is per-account too — sum raw tokens per tier across hosts.
    let mut mix: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (tier, v) in local
        .model_mix
        .iter()
        .chain(remotes.iter().flat_map(|r| r.model_mix.iter()))
    {
        *mix.entry(tier.clone()).or_insert(0) += v;
    }
    let model_mix = mix.into_iter().collect();

    // Usage % is account-wide — keep the freshest reading across every host.
    let weekly_usage_pct = std::iter::once(local_weekly)
        .chain(remotes.iter().map(|r| r.weekly_usage_pct))
        .flatten()
        .max_by_key(|u| u.at_ms);
    let window_usage_pct = std::iter::once(local_window)
        .chain(remotes.iter().map(|r| r.window_usage_pct))
        .flatten()
        .max_by_key(|u| u.at_ms);

    Snapshot {
        generated_at: local.generated_at,
        sessions,
        alerts,
        totals,
        usage_buckets,
        rate_limits,
        limit_hits,
        model_mix,
        weekly_usage_pct,
        window_usage_pct,
        governor: None,
        pacing: None,
    }
}

/// Local first, then remote hosts, then cloud.
fn host_rank(h: &Host) -> u8 {
    match h {
        Host::Local => 0,
        Host::Remote { .. } => 1,
        Host::Cloud => 2,
    }
}

/// Load remote definitions from a JSON array file. Missing/invalid → empty.
pub fn load_remotes(path: &Path) -> Vec<RemoteDef> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use std::sync::Mutex;

    /// A runner that returns a canned string and records what it saw.
    struct MockRunner {
        output: String,
        seen: Mutex<Vec<(Vec<String>, bool)>>,
    }
    impl CommandRunner for MockRunner {
        fn run(
            &self,
            argv: &[String],
            stdin: Option<&str>,
            _t: Duration,
        ) -> anyhow::Result<String> {
            self.seen
                .lock()
                .unwrap()
                .push((argv.to_vec(), stdin.is_some()));
            Ok(self.output.clone())
        }
    }

    fn snap_with_session(name: &str, tpm: f64) -> Snapshot {
        let mut s = Snapshot::empty(1000);
        s.sessions.push(Session {
            id: format!("id-{name}"),
            name: name.into(),
            title: None,
            cwd: "/x".into(),
            pid: Some(1),
            kind: "interactive".into(),
            entrypoint: "cli".into(),
            version: "1".into(),
            model: None,
            state: SessionState::Running,
            started_at: Some(0),
            last_activity: Some(0),
            last_user_turn: None,
            priority_override: None,
            tokens: TokenLedger {
                input: 100,
                output: 200,
                cache_read: 300,
                ..Default::default()
            },
            tokens_per_min: tpm,
            cpu_pct: 0.0,
            rss_mb: 0,
            agents: vec![],
            tasks: vec![],
            watchers: vec![],
            activity: vec![],
            processes: vec![],
            host: Host::Local,
            remote_name: None,
        });
        s
    }

    #[test]
    fn ssh_default_is_zero_install_probe() {
        let def = RemoteDef {
            name: "demo-host".into(),
            kind: RemoteKind::Ssh,
            target: Some("user@demo-host".into()),
            fetch: vec![],
            cancel: vec![],
        };
        let (argv, stdin) = def.fetch_plan().unwrap();
        // Pipes the python probe over ssh — nothing installed remotely.
        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"BatchMode=yes".to_string()), "must not hang on auth");
        assert!(argv.contains(&"user@demo-host".to_string()));
        assert_eq!(argv[argv.len() - 2..], ["python3", "-"]);
        assert_eq!(stdin, Some(PROBE_PY));
        assert!(PROBE_PY.contains("Snapshot"), "probe source embedded");

        // An explicit fetch overrides the probe.
        let custom = RemoteDef {
            fetch: vec!["my-script".into()],
            ..def
        };
        let (argv, stdin) = custom.fetch_plan().unwrap();
        assert_eq!(argv, vec!["my-script"]);
        assert_eq!(stdin, None);
    }

    #[test]
    fn fetch_retags_sessions_with_remote_host() {
        let remote_snap = snap_with_session("worker", 5000.0);
        let runner = MockRunner {
            output: serde_json::to_string(&remote_snap).unwrap(),
            seen: Mutex::new(vec![]),
        };
        let def = RemoteDef {
            name: "demo-host".into(),
            kind: RemoteKind::Ssh,
            target: Some("user@demo-host".into()),
            fetch: vec![],
            cancel: vec![],
        };
        let fetched = fetch_remote(&def, &runner, Duration::from_secs(1)).unwrap();
        assert_eq!(fetched.sessions.len(), 1);
        match &fetched.sessions[0].host {
            Host::Remote { name, ssh_target } => {
                assert_eq!(name, "demo-host");
                assert_eq!(ssh_target, "user@demo-host");
            }
            other => panic!("expected Remote host, got {other:?}"),
        }
        // It ran the default ssh probe with stdin payload.
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen[0].0[0], "ssh");
        assert!(seen[0].1, "probe should be fed via stdin");
    }

    #[test]
    fn merge_aggregates_hosts_and_totals() {
        let local = snap_with_session("local1", 1000.0);
        let mut remote = snap_with_session("worker", 5000.0);
        remote.sessions[0].host = Host::Remote {
            name: "demo-host".into(),
            ssh_target: "demo-host".into(),
        };
        let merged = merge(local, &[remote]);
        assert_eq!(merged.sessions.len(), 2);
        assert_eq!(merged.totals.active_sessions, 2);
        assert_eq!(merged.totals.tokens_per_min, 6000.0);
        // Local sorts before remote.
        assert!(matches!(merged.sessions[0].host, Host::Local));
        assert!(matches!(merged.sessions[1].host, Host::Remote { .. }));
    }

    #[test]
    fn cloud_host_and_cancel_substitution() {
        let def = RemoteDef {
            name: "cloud".into(),
            kind: RemoteKind::Cloud,
            target: None,
            fetch: vec!["my-cloud-script".into()],
            cancel: vec!["my-cloud-cancel".into(), "--id".into(), "{id}".into()],
        };
        assert!(matches!(def.host(), Host::Cloud));
        assert_eq!(
            def.cancel_argv("routine-7").unwrap(),
            vec!["my-cloud-cancel", "--id", "routine-7"]
        );
        let noncancel = RemoteDef {
            cancel: vec![],
            ..def.clone()
        };
        assert!(noncancel.cancel_argv("x").is_none());
    }
}
