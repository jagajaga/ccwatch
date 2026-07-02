//! The stateful engine. It holds everything that must persist across refreshes
//! — transcript byte-offset watermarks and per-session accumulators — so each
//! refresh reads only newly-appended transcript bytes and folds them into
//! running totals.
//!
//! `refresh(now_ms)` takes the clock explicitly so tests are fully
//! deterministic; `refresh_now()` is the live convenience wrapper.

use crate::collectors::{hooks, processes::ProcessProbe, sessions, tasks, transcripts};
use crate::collectors::transcripts::TranscriptEvent;
use crate::config::Config;
use crate::leaks;
use crate::model::*;
use crate::paths::{self, Paths};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

/// One folded assistant message, kept for windowed rate math.
#[derive(Clone, Copy, Debug)]
struct TokenEvent {
    ts_ms: i64,
    total: u64,
    input: u64,
    cache_read: u64,
}

#[derive(Clone, Debug)]
struct AgentAccum {
    id: String,
    subagent_type: String,
    description: String,
    model: Option<String>,
    started_at: Option<i64>,
    finished: bool,
    /// Background launch: only a task-notification finishes it — the immediate
    /// tool_result is just the "started" acknowledgement.
    background: bool,
    children: Vec<AgentAccum>,
}

impl AgentAccum {
    fn to_model(&self) -> Agent {
        Agent {
            id: self.id.clone(),
            subagent_type: self.subagent_type.clone(),
            description: self.description.clone(),
            model: self.model.clone(),
            state: if self.finished {
                AgentState::Finished
            } else {
                AgentState::Running
            },
            started_at: self.started_at,
            tokens: TokenLedger::default(),
            tokens_per_min: 0.0,
            children: self.children.iter().map(AgentAccum::to_model).collect(),
        }
    }
}

/// Per-session persistent accumulator.
#[derive(Default)]
struct SessionAccum {
    ledger: TokenLedger,
    events: VecDeque<TokenEvent>,
    last_user_turn: Option<i64>,
    last_activity: Option<i64>,
    model: Option<String>,
    agents: Vec<AgentAccum>,
    /// Aggregated ScheduleWakeup ("loop") watcher state.
    loop_count: u64,
    loop_last: Option<i64>,
    loop_next_wake: Option<i64>,
    loop_reason: Option<String>,
}

impl SessionAccum {
    /// Mark an agent (anywhere in the tree) finished by tool_use id.
    /// `authoritative` (task-notification) finishes background agents too;
    /// a plain tool_result only finishes foreground ones.
    fn finish_agent(&mut self, id: &str, authoritative: bool) {
        fn walk(agents: &mut [AgentAccum], id: &str, authoritative: bool) -> bool {
            for a in agents.iter_mut() {
                if a.id == id {
                    if authoritative || !a.background {
                        a.finished = true;
                    }
                    return true;
                }
                if walk(&mut a.children, id, authoritative) {
                    return true;
                }
            }
            false
        }
        walk(&mut self.agents, id, authoritative);
    }

    /// Attach a newly-started agent. Non-sidechain agents are top-level;
    /// sidechain agents nest under the most recent running top-level agent.
    fn add_agent(&mut self, agent: AgentAccum, is_sidechain: bool) {
        if is_sidechain {
            if let Some(parent) = self.agents.iter_mut().rev().find(|a| !a.finished) {
                parent.children.push(agent);
                return;
            }
        }
        self.agents.push(agent);
    }

    fn prune(&mut self, now_ms: i64, retain_secs: i64) {
        let cutoff = now_ms - retain_secs * 1000;
        while let Some(front) = self.events.front() {
            if front.ts_ms < cutoff {
                self.events.pop_front();
            } else {
                break;
            }
        }
    }

    /// Sum of `total` tokens within the window ending at `now_ms`.
    fn window_sum(&self, now_ms: i64, window_secs: i64) -> (u64, u64, u64) {
        let cutoff = now_ms - window_secs * 1000;
        let mut total = 0u64;
        let mut input = 0u64;
        let mut cache_read = 0u64;
        for e in self.events.iter().filter(|e| e.ts_ms >= cutoff) {
            total += e.total;
            input += e.input;
            cache_read += e.cache_read;
        }
        (total, input, cache_read)
    }
}

pub struct Engine {
    paths: Paths,
    config: Config,
    probe: ProcessProbe,
    watermarks: HashMap<PathBuf, u64>,
    accums: HashMap<String, SessionAccum>,
    /// Timestamps of observed 429 rate-limit events (pruned to retention).
    rate_limits: VecDeque<i64>,
    /// Last time ended-session transcripts were rescanned (epoch ms).
    last_ended_scan: i64,
}

impl Engine {
    pub fn new(paths: Paths, config: Config) -> Self {
        Engine {
            paths,
            config,
            probe: ProcessProbe::new(),
            watermarks: HashMap::new(),
            accums: HashMap::new(),
            rate_limits: VecDeque::new(),
            // i64::MIN would overflow the subtraction; "long ago" is enough.
            last_ended_scan: -1,
        }
    }

    /// Observed 429 timestamps within retention (for budget learning).
    pub fn rate_limit_ts(&self) -> Vec<i64> {
        self.rate_limits.iter().copied().collect()
    }

    pub fn with_defaults() -> Self {
        let paths = Paths::discover();
        let config = Config::load(&paths.config_file());
        Engine::new(paths, config)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Refresh using the wall clock.
    pub fn refresh_now(&mut self) -> Snapshot {
        let now = chrono::Utc::now().timestamp_millis();
        self.refresh(now)
    }

    /// Rebuild the snapshot as of `now_ms`.
    pub fn refresh(&mut self, now_ms: i64) -> Snapshot {
        // Index transcripts by session id (filename stem).
        let transcript_index = self.index_transcripts();

        // Which sessions exist right now. Only their pids are probed —
        // a full process-table scan every tick is wasted CPU.
        let metas = sessions::read_sessions(&self.paths.sessions());
        let pids: Vec<i32> = metas.iter().filter_map(|m| m.pid).collect();
        self.probe.refresh_pids(&pids);

        // Fold any new transcript bytes for each known session.
        for meta in &metas {
            if let Some(path) = transcript_index.get(&meta.session_id) {
                self.ingest_transcript(&meta.session_id, path.clone());
            }
        }

        // Also ingest recently-modified transcripts of sessions that no longer
        // exist — their burn still counts against the account's plan window.
        // Their files don't change often, so stat-ing hundreds of transcripts
        // is throttled to every 30 s rather than every tick.
        let retention_ms = self.config.retention_secs() * 1000;
        if now_ms - self.last_ended_scan >= 30_000 {
            self.last_ended_scan = now_ms;
            let known: std::collections::HashSet<&str> =
                metas.iter().map(|m| m.session_id.as_str()).collect();
            let recent: Vec<(String, PathBuf)> = transcript_index
                .iter()
                .filter(|(sid, path)| {
                    !known.contains(sid.as_str())
                        && std::fs::metadata(path)
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.elapsed().ok())
                            .map(|age| (age.as_millis() as i64) < retention_ms)
                            .unwrap_or(false)
                })
                .map(|(sid, path)| (sid.clone(), path.clone()))
                .collect();
            for (sid, path) in recent {
                self.ingest_transcript(&sid, path);
            }
        }

        // Prune stale rate-limit events.
        while let Some(&front) = self.rate_limits.front() {
            if front < now_ms - retention_ms {
                self.rate_limits.pop_front();
            } else {
                break;
            }
        }

        let mut out_sessions = Vec::new();
        let mut alerts = Vec::new();

        for meta in &metas {
            let alive = meta.pid.map(|p| self.probe.is_alive(p)).unwrap_or(false);
            if !alive {
                // Ended sessions are never displayed.
                continue;
            }
            let session_id = meta.session_id.clone();

            // Ensure an accumulator exists even with no transcript yet.
            self.accums.entry(session_id.clone()).or_default();
            let accum = self.accums.get_mut(&session_id).unwrap();
            accum.prune(now_ms, self.config.retention_secs());

            let (win_total, win_input, win_cache_read) =
                accum.window_sum(now_ms, self.config.rate_window_secs);
            let window_min = self.config.rate_window_secs as f64 / 60.0;
            let tokens_per_min = if window_min > 0.0 {
                win_total as f64 / window_min
            } else {
                0.0
            };
            let input_per_min = if window_min > 0.0 {
                win_input as f64 / window_min
            } else {
                0.0
            };
            let window_cache_ratio = {
                let denom = win_input + win_cache_read;
                if denom == 0 {
                    None
                } else {
                    Some(win_cache_read as f64 / denom as f64)
                }
            };

            let last_activity = accum.last_activity.or(meta.started_at);
            let state = if let Some(la) = last_activity {
                if now_ms - la <= self.config.idle_secs * 1000 {
                    SessionState::Running
                } else {
                    SessionState::Idle
                }
            } else {
                SessionState::Running
            };

            let stat = meta
                .pid
                .and_then(|p| self.probe.stat(p))
                .unwrap_or_default();

            // Agents (clone tree into model form).
            let agents: Vec<Agent> = accum.agents.iter().map(AgentAccum::to_model).collect();
            let agent_starts_in_window = count_recent_agent_starts(
                &accum.agents,
                now_ms,
                self.config.agent_storm_window_secs,
            );

            // Watchers: schedule-wakeup loop + hooks (global + project).
            let mut watchers = Vec::new();
            if accum.loop_count > 0 {
                watchers.push(Watcher {
                    kind: WatcherKind::Loop,
                    name: "ScheduleWakeup".to_string(),
                    detail: accum
                        .loop_reason
                        .clone()
                        .unwrap_or_else(|| "self-paced loop".to_string()),
                    schedule: Some("self-paced".to_string()),
                    last_fired: accum.loop_last,
                    fired_count: accum.loop_count,
                    next_wake: accum.loop_next_wake,
                    running: true,
                    pid: None,
                });
            }
            // Snapshot the scalar leak inputs before dropping the &mut borrow.
            let leak_inputs = LeakInputs {
                session_id: session_id.clone(),
                name: meta.name.clone(),
                tokens_per_min,
                input_per_min,
                window_cache_ratio,
                last_user_turn: accum.last_user_turn,
                state,
                agent_starts_in_window,
            };
            let ledger = accum.ledger;

            // Hooks are outside the accum borrow.
            watchers.extend(hooks::read_hooks(&self.paths.settings()));
            let cwd = PathBuf::from(&meta.cwd);
            watchers.extend(hooks::read_hooks(&paths::project_settings(&cwd)));

            // Tasks.
            let session_tasks = tasks::read_tasks(&self.paths.tasks_for(&session_id));

            let name = if meta.name.is_empty() {
                session_id.clone()
            } else {
                meta.name.clone()
            };

            out_sessions.push(Session {
                id: session_id.clone(),
                name,
                cwd: meta.cwd.clone(),
                pid: meta.pid,
                kind: if meta.kind.is_empty() {
                    "interactive".into()
                } else {
                    meta.kind.clone()
                },
                entrypoint: meta.entrypoint.clone(),
                version: meta.version.clone(),
                model: self.accums.get(&session_id).and_then(|a| a.model.clone()),
                state,
                started_at: meta.started_at,
                last_activity,
                tokens: ledger,
                tokens_per_min,
                cpu_pct: stat.cpu_pct,
                rss_mb: stat.rss_mb,
                agents,
                tasks: session_tasks,
                watchers,
                host: Host::Local,
                remote_name: None,
            });

            alerts.extend(self.detect_leaks(&leak_inputs, now_ms));
        }

        // Sort: highest burn first, so the eye lands on the leaker.
        out_sessions.sort_by(|a, b| {
            b.tokens_per_min
                .partial_cmp(&a.tokens_per_min)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        alerts.sort_by_key(|a| std::cmp::Reverse(a.severity));

        let totals = compute_totals(&out_sessions);

        // Billable usage buckets across ALL accumulators (including ended
        // sessions) — the governor's raw fuel data. Prune ended-session accums
        // here since the display loop above only prunes live ones.
        for accum in self.accums.values_mut() {
            accum.prune(now_ms, self.config.retention_secs());
        }
        let mut bucket_map: std::collections::BTreeMap<i64, u64> = Default::default();
        for accum in self.accums.values() {
            for e in &accum.events {
                *bucket_map
                    .entry(crate::governor::bucket_of(e.ts_ms))
                    .or_insert(0) += e.total;
            }
        }

        Snapshot {
            generated_at: now_ms,
            sessions: out_sessions,
            alerts,
            totals,
            usage_buckets: bucket_map.into_iter().collect(),
            rate_limits: self.rate_limit_ts(),
            governor: None,
        }
    }

    fn detect_leaks(&self, inp: &LeakInputs, now_ms: i64) -> Vec<Alert> {
        let cfg = &self.config;
        let mut out = Vec::new();
        let subject = inp.name.clone();

        if leaks::is_runaway(
            inp.tokens_per_min,
            inp.last_user_turn.map(|t| (now_ms - t) / 1000),
            cfg,
        ) {
            let mins = inp
                .last_user_turn
                .map(|t| (now_ms - t) / 60000)
                .unwrap_or(0);
            out.push(Alert {
                severity: Severity::Critical,
                kind: AlertKind::RunawayLoop,
                subject: subject.clone(),
                session_id: inp.session_id.clone(),
                message: format!(
                    "{:.0}k tok/min · no user turn {}m",
                    inp.tokens_per_min / 1000.0,
                    mins
                ),
                since_ms: inp.last_user_turn.unwrap_or(now_ms),
            });
        }

        if leaks::is_cache_bleed(inp.input_per_min, inp.window_cache_ratio, cfg) {
            let pct = inp.window_cache_ratio.unwrap_or(0.0) * 100.0;
            out.push(Alert {
                severity: Severity::Warn,
                kind: AlertKind::CacheBleed,
                subject: subject.clone(),
                session_id: inp.session_id.clone(),
                message: format!(
                    "cache-read {:.0}% · input {:.0}k/min",
                    pct,
                    inp.input_per_min / 1000.0
                ),
                since_ms: now_ms,
            });
        }

        if leaks::is_zombie(matches!(inp.state, SessionState::Idle), inp.tokens_per_min, cfg) {
            out.push(Alert {
                severity: Severity::Warn,
                kind: AlertKind::ZombieSession,
                subject: subject.clone(),
                session_id: inp.session_id.clone(),
                message: format!("idle but burning {:.0}k tok/min", inp.tokens_per_min / 1000.0),
                since_ms: now_ms,
            });
        }

        if leaks::is_agent_storm(inp.agent_starts_in_window, cfg) {
            out.push(Alert {
                severity: Severity::Warn,
                kind: AlertKind::AgentStorm,
                subject: subject.clone(),
                session_id: inp.session_id.clone(),
                message: format!(
                    "{} agents spawned in {}s",
                    inp.agent_starts_in_window, cfg.agent_storm_window_secs
                ),
                since_ms: now_ms,
            });
        }

        out
    }

    /// Read newly-appended bytes for a session's transcript and fold events.
    fn ingest_transcript(&mut self, session_id: &str, path: PathBuf) {
        let watermark = *self.watermarks.get(&path).unwrap_or(&0);
        let (lines, new_watermark) = match read_new_lines(&path, watermark) {
            Some(v) => v,
            None => return,
        };
        self.watermarks.insert(path, new_watermark);

        let accum = self.accums.entry(session_id.to_string()).or_default();
        for line in lines {
            for ev in transcripts::parse_line(&line) {
                match ev {
                    TranscriptEvent::Assistant {
                        ts_ms,
                        model,
                        usage,
                        ..
                    } => {
                        accum.ledger.add(&usage);
                        // Skip internal markers like "<synthetic>".
                        if let Some(m) = model {
                            if !m.starts_with('<') {
                                accum.model = Some(m);
                            }
                        }
                        if let Some(ts) = ts_ms {
                            accum.last_activity = Some(ts.max(accum.last_activity.unwrap_or(0)));
                            accum.events.push_back(TokenEvent {
                                ts_ms: ts,
                                // Burn rate tracks billable tokens, not cheap
                                // cache reads (those go in `cache_read` for the
                                // bleed heuristic).
                                total: usage.billable(),
                                input: usage.input,
                                cache_read: usage.cache_read,
                            });
                        }
                    }
                    TranscriptEvent::UserTurn { ts_ms } => {
                        if let Some(ts) = ts_ms {
                            accum.last_user_turn =
                                Some(ts.max(accum.last_user_turn.unwrap_or(0)));
                            accum.last_activity = Some(ts.max(accum.last_activity.unwrap_or(0)));
                        }
                    }
                    TranscriptEvent::AgentStart {
                        id,
                        subagent_type,
                        description,
                        model,
                        ts_ms,
                        is_sidechain,
                        background,
                    } => {
                        accum.add_agent(
                            AgentAccum {
                                id,
                                subagent_type,
                                description,
                                model,
                                started_at: ts_ms,
                                finished: false,
                                background,
                                children: Vec::new(),
                            },
                            is_sidechain,
                        );
                    }
                    TranscriptEvent::ToolResult {
                        tool_use_id,
                        notification,
                    } => {
                        accum.finish_agent(&tool_use_id, notification);
                    }
                    TranscriptEvent::RateLimited { ts_ms } => {
                        if let Some(ts) = ts_ms {
                            self.rate_limits.push_back(ts);
                        }
                    }
                    TranscriptEvent::ScheduleWakeup {
                        ts_ms,
                        delay_secs,
                        reason,
                    } => {
                        accum.loop_count += 1;
                        accum.loop_last = ts_ms.or(accum.loop_last);
                        accum.loop_next_wake = match (ts_ms, delay_secs) {
                            (Some(t), Some(d)) => Some(t + d * 1000),
                            _ => accum.loop_next_wake,
                        };
                        if reason.is_some() {
                            accum.loop_reason = reason;
                        }
                    }
                }
            }
        }
    }

    /// Map session id → transcript path by scanning `projects/*/`.
    fn index_transcripts(&self) -> HashMap<String, PathBuf> {
        let mut index = HashMap::new();
        let Ok(projects) = std::fs::read_dir(self.paths.projects()) else {
            return index;
        };
        for proj in projects.flatten() {
            let Ok(files) = std::fs::read_dir(proj.path()) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        index.insert(stem.to_string(), p);
                    }
                }
            }
        }
        index
    }
}

/// Scalar inputs to leak detection, snapshotted to avoid borrow conflicts.
struct LeakInputs {
    session_id: String,
    name: String,
    tokens_per_min: f64,
    input_per_min: f64,
    window_cache_ratio: Option<f64>,
    last_user_turn: Option<i64>,
    state: SessionState,
    agent_starts_in_window: usize,
}

fn count_recent_agent_starts(agents: &[AgentAccum], now_ms: i64, window_secs: i64) -> usize {
    let cutoff = now_ms - window_secs * 1000;
    let mut n = 0;
    fn walk(agents: &[AgentAccum], cutoff: i64, n: &mut usize) {
        for a in agents {
            if a.started_at.map(|t| t >= cutoff).unwrap_or(false) {
                *n += 1;
            }
            walk(&a.children, cutoff, n);
        }
    }
    walk(agents, cutoff, &mut n);
    n
}

fn compute_totals(sessions: &[Session]) -> Totals {
    let mut ledger = TokenLedger::default();
    let mut tpm = 0.0;
    for s in sessions {
        ledger.add(&s.tokens);
        tpm += s.tokens_per_min;
    }
    Totals {
        active_sessions: sessions.len(),
        tokens_per_min: tpm,
        total_tokens: ledger.grand_total(),
        cache_hit_pct: ledger.cache_hit_ratio().unwrap_or(0.0) * 100.0,
    }
}

/// Read bytes from `watermark` to EOF, returning complete lines and the new
/// watermark (advanced only past the last complete line, so a half-written
/// trailing line is re-read next time). `None` on I/O error.
fn read_new_lines(path: &std::path::Path, watermark: u64) -> Option<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len < watermark {
        // File truncated/rotated — start over.
        return read_new_lines(path, 0);
    }
    if len == watermark {
        return Some((Vec::new(), watermark));
    }
    file.seek(SeekFrom::Start(watermark)).ok()?;
    let mut buf = Vec::with_capacity((len - watermark) as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // Advance only to the last newline.
    let last_nl = text.rfind('\n');
    let (consumed, complete) = match last_nl {
        Some(idx) => (idx + 1, &text[..=idx]),
        None => return Some((Vec::new(), watermark)), // no complete line yet
    };
    let lines: Vec<String> = complete
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    Some((lines, watermark + consumed as u64))
}
