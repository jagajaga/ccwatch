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

/// An agent with no sign of life for this long is treated as finished, even
/// without an authoritative completion marker — a safety net for agents whose
/// session crashed or ended before the marker was written. Generous so a
/// live-but-quiet background agent isn't misreported as done.
const AGENT_STALE_MS: i64 = 10 * 60_000;

/// One folded assistant message, kept for windowed rate math.
#[derive(Clone, Copy, Debug)]
struct TokenEvent {
    ts_ms: i64,
    /// Billable tokens (input + output + cache-creation) — drives the Governor
    /// and everything that projects rate-limit consumption.
    total: u64,
    /// "Work" tokens (input + output only) — the displayed session/agent burn
    /// rate, which stays intuitive instead of being dominated by cache churn.
    work: u64,
    input: u64,
    cache_read: u64,
    /// The model tier of *this* message (not the session's latest), so the
    /// governor weights each message by what actually produced it.
    tier: &'static str,
    /// Opus-equivalent weight for `tier` at ingest time.
    weight: f64,
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
    fn to_model(
        &self,
        data: &HashMap<String, AgentData>,
        now_ms: i64,
        rate_window_secs: i64,
        stale_ms: i64,
    ) -> Agent {
        let d = data.get(&self.id);
        let tokens = d.map(|d| d.ledger).unwrap_or_default();
        let tokens_per_min = d
            .map(|d| {
                let cutoff = now_ms - rate_window_secs * 1000;
                let sum: u64 = d
                    .events
                    .iter()
                    .filter(|e| e.ts_ms >= cutoff)
                    .map(|e| e.work)
                    .sum();
                sum as f64 / (rate_window_secs as f64 / 60.0)
            })
            .unwrap_or(0.0);
        let mut activity: Vec<Activity> = d
            .map(|d| {
                d.pending
                    .values()
                    .filter(|(_, _, ts)| now_ms - ts < 30 * 60_000)
                    .map(|(tool, detail, ts)| Activity {
                        tool: tool.clone(),
                        detail: detail.clone(),
                        since_ms: *ts,
                    })
                    .collect()
            })
            .unwrap_or_default();
        activity.sort_by_key(|a| a.since_ms);
        activity.truncate(4);
        let last_act = d.and_then(|d| d.last_activity);
        let recent = last_act.is_some_and(|la| now_ms - la <= stale_ms)
            || self.started_at.is_some_and(|s| now_ms - s <= stale_ms);
        let children: Vec<Agent> = self
            .children
            .iter()
            .map(|c| c.to_model(data, now_ms, rate_window_secs, stale_ms))
            .collect();
        // An agent that is *burning tokens right now* is running — even if a
        // (sometimes premature) completion marker was seen; the sidechain keeps
        // generating. Otherwise honor the finish flag, and treat agents with no
        // recent life as finished (guards against completion markers that never
        // arrive from crashed/ended sessions leaving phantom "running" agents). A
        // grouping node (e.g. a Workflow) is running whenever any child still is.
        let any_child_running = children.iter().any(|c| matches!(c.state, AgentState::Running));
        let running = tokens_per_min > 0.0 || (!self.finished && recent) || any_child_running;
        Agent {
            id: self.id.clone(),
            subagent_type: self.subagent_type.clone(),
            description: self.description.clone(),
            model: d.and_then(|d| d.model.clone()).or_else(|| self.model.clone()),
            state: if running {
                AgentState::Running
            } else {
                AgentState::Finished
            },
            started_at: self.started_at,
            tokens,
            tokens_per_min,
            activity,
            last_activity: last_act,
            children,
        }
    }
}

/// The `meta.json` beside a subagent sidechain: who launched it and what it is.
#[derive(Clone)]
struct SidechainMeta {
    tool_use_id: String,
    agent_type: String,
    description: String,
}

/// Live data read from one subagent's own sidechain transcript.
#[derive(Default)]
struct AgentData {
    ledger: TokenLedger,
    events: VecDeque<TokenEvent>,
    model: Option<String>,
    pending: HashMap<String, (String, String, i64)>,
    last_activity: Option<i64>,
}

/// Per-session persistent accumulator.
#[derive(Default)]
struct SessionAccum {
    ledger: TokenLedger,
    /// In-flight tool calls: id -> (tool, detail, started_ms). Removed when
    /// the matching tool_result arrives.
    pending_tools: HashMap<String, (String, String, i64)>,
    /// Per-agent sidechain data, keyed by the launching tool_use id.
    agent_data: HashMap<String, AgentData>,
    events: VecDeque<TokenEvent>,
    last_user_turn: Option<i64>,
    last_activity: Option<i64>,
    model: Option<String>,
    ai_title: Option<String>,
    custom_title: Option<String>,
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

    /// Ensure an agent exists for a sidechain we're ingesting. Nested subagents
    /// (spawnDepth ≥ 1, launched from inside a Workflow/agent rather than the
    /// main transcript) never emit an `AgentStart` we see, so without this their
    /// burn rolls into the session with no visible agent. Synthesize one from
    /// its meta.json so live subagent work is always shown.
    fn ensure_agent(&mut self, id: &str, agent_type: &str, description: &str) {
        fn exists(agents: &[AgentAccum], id: &str) -> bool {
            agents.iter().any(|a| a.id == id || exists(&a.children, id))
        }
        if exists(&self.agents, id) {
            return;
        }
        self.add_agent(
            AgentAccum {
                id: id.to_string(),
                subagent_type: agent_type.to_string(),
                description: description.to_string(),
                model: None,
                started_at: None,
                finished: false,
                background: true,
                children: Vec::new(),
            },
            true,
        );
    }

    /// Ensure a synthetic `Workflow` parent (id = `wf_…`) exists with the given
    /// children nested under it. The Workflow tool spawns a fleet whose agents
    /// have no launching tool call in the main transcript, so we group them here
    /// under one node — matching Claude's own background-tasks view.
    fn ensure_workflow(&mut self, wf_id: &str, label: &str, children: &[(String, String)]) {
        fn find<'a>(agents: &'a mut [AgentAccum], id: &str) -> Option<&'a mut AgentAccum> {
            for a in agents.iter_mut() {
                if a.id == id {
                    return Some(a);
                }
                if let Some(f) = find(&mut a.children, id) {
                    return Some(f);
                }
            }
            None
        }
        if find(&mut self.agents, wf_id).is_none() {
            self.agents.push(AgentAccum {
                id: wf_id.to_string(),
                subagent_type: "Workflow".to_string(),
                description: label.to_string(),
                model: None,
                started_at: None,
                finished: false,
                background: true,
                children: Vec::new(),
            });
        }
        let parent = find(&mut self.agents, wf_id).expect("just inserted");
        for (cid, desc) in children {
            if !parent.children.iter().any(|c| &c.id == cid) {
                parent.children.push(AgentAccum {
                    id: cid.clone(),
                    subagent_type: "workflow-subagent".to_string(),
                    description: desc.clone(),
                    model: None,
                    started_at: None,
                    finished: false,
                    background: true,
                    children: Vec::new(),
                });
            }
        }
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
        for d in self.agent_data.values_mut() {
            while let Some(front) = d.events.front() {
                if front.ts_ms < cutoff {
                    d.events.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// Windowed sums ending at `now_ms`: (work, input, cache_read). The
    /// displayed burn rate uses `work` (input+output); the Governor sums
    /// billable `total` separately from its own buckets.
    fn window_sum(&self, now_ms: i64, window_secs: i64) -> (u64, u64, u64) {
        let cutoff = now_ms - window_secs * 1000;
        let mut work = 0u64;
        let mut input = 0u64;
        let mut cache_read = 0u64;
        for e in self.events.iter().filter(|e| e.ts_ms >= cutoff) {
            work += e.work;
            input += e.input;
            cache_read += e.cache_read;
        }
        (work, input, cache_read)
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
    /// Last full process-table scan (epoch ms) — needed for child discovery.
    last_full_scan: i64,
    /// agent sidechain path -> its meta.json (launching tool_use id, type, desc).
    agent_meta: HashMap<PathBuf, Option<SidechainMeta>>,
    /// workflow-agent sidechain path -> its resolved description (first prompt).
    /// Cached so we read each fleet agent's opening line only once.
    wf_child_desc: HashMap<PathBuf, String>,
    /// Parsed limit-hit markers (session + weekly), pruned to ~2 weeks.
    limit_hits: Vec<crate::model::LimitHit>,
    /// Latest usage-% banners Claude Code printed, per period.
    weekly_usage: Option<crate::model::UsagePct>,
    window_usage: Option<crate::model::UsagePct>,
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
            last_full_scan: -1,
            agent_meta: HashMap::new(),
            wf_child_desc: HashMap::new(),
            limit_hits: Vec::new(),
            weekly_usage: None,
            window_usage: None,
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
        // Child-process discovery needs the whole table; do that every ~5s and
        // the cheap targeted refresh in between.
        if now_ms - self.last_full_scan >= 5_000 {
            self.last_full_scan = now_ms;
            self.probe.refresh();
        } else {
            self.probe.refresh_pids(&pids);
        }

        // Fold any new transcript bytes for each known session, then their
        // subagents' own sidechain transcripts
        // (`<project>/<sessionId>/subagents/agent-*.jsonl`).
        for meta in &metas {
            if let Some(path) = transcript_index.get(&meta.session_id) {
                self.ingest_transcript(&meta.session_id, path.clone());
            }
        }

        // Discover every `<project>/<sid>/subagents/` directory across all
        // project dirs and ingest it. This is independent of which sessions are
        // "known" — worktree sessions keep their subagents in a different
        // project dir than their main transcript, and may not be in `metas` at
        // all — so a global scan is the only way to attribute subagent tokens.
        for (sid, dir) in self.discover_subagent_dirs() {
            self.ingest_agent_sidechains(&sid, &dir);
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
        // Keep limit-hit markers for two weeks (weekly window + slack).
        let hit_cutoff = now_ms - 15 * 24 * 3_600_000;
        self.limit_hits.retain(|h| h.at_ms >= hit_cutoff);

        // Drop accumulators for sessions that are neither alive nor recently
        // active. Without this, every historical session the sidechain scan ever
        // touched would linger in memory with its whole agent tree, growing the
        // daemon's RSS (and, via alive long-runners, the snapshots) unbounded.
        {
            let alive: std::collections::HashSet<&str> = metas
                .iter()
                .filter(|m| m.pid.map(|p| self.probe.is_alive(p)).unwrap_or(false))
                .map(|m| m.session_id.as_str())
                .collect();
            let acc_cutoff = now_ms - retention_ms;
            self.accums.retain(|sid, a| {
                alive.contains(sid.as_str())
                    || a.last_activity.map(|la| la >= acc_cutoff).unwrap_or(false)
            });
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

            let (win_work, win_input, win_cache_read) =
                accum.window_sum(now_ms, self.config.rate_window_secs);
            let window_min = self.config.rate_window_secs as f64 / 60.0;
            let tokens_per_min = if window_min > 0.0 {
                win_work as f64 / window_min
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

            let stat = meta
                .pid
                .and_then(|p| self.probe.stat(p))
                .unwrap_or_default();

            // In-flight activity: pending tools, oldest first. Entries whose
            // results were lost (compaction, crashes) expire after 30 min.
            accum
                .pending_tools
                .retain(|_, (_, _, ts)| now_ms - *ts < 30 * 60_000);
            let mut activity: Vec<Activity> = accum
                .pending_tools
                .values()
                .map(|(tool, detail, ts)| Activity {
                    tool: tool.clone(),
                    detail: detail.clone(),
                    since_ms: *ts,
                })
                .collect();
            activity.sort_by_key(|a| a.since_ms);
            activity.truncate(6);

            // State: generating recently → running; otherwise a pending tool
            // call means "waiting" (a build, a permission prompt, a question)
            // — which burns time but zero tokens; else idle.
            let state = match last_activity {
                Some(la) if now_ms - la <= self.config.idle_secs * 1000 => SessionState::Running,
                _ if !activity.is_empty() => SessionState::Waiting,
                Some(_) => SessionState::Idle,
                None => SessionState::Running,
            };

            // Agents (clone tree into model form, enriched from sidechains).
            // Drop bare `Workflow` launch nodes with no fleet of their own — the
            // real fleet is shown under the synthetic `wf_…` node instead.
            let agents: Vec<Agent> = accum
                .agents
                .iter()
                .map(|a| a.to_model(&accum.agent_data, now_ms, self.config.rate_window_secs, AGENT_STALE_MS))
                .filter(|a| !(a.subagent_type == "Workflow" && a.children.is_empty()))
                .collect();
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

            let title = {
                let a = self.accums.get(&session_id);
                a.and_then(|a| a.custom_title.clone())
                    .or_else(|| a.and_then(|a| a.ai_title.clone()))
            };
            out_sessions.push(Session {
                id: session_id.clone(),
                name,
                title,
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
                last_user_turn: leak_inputs.last_user_turn,
                priority_override: None, // stamped by the daemon from its override map
                tokens: ledger,
                tokens_per_min,
                cpu_pct: stat.cpu_pct,
                rss_mb: stat.rss_mb,
                agents,
                tasks: session_tasks,
                watchers,
                activity,
                processes: meta
                    .pid
                    .map(|p| self.probe.children_of(p, 12))
                    .unwrap_or_default(),
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
        // usage_buckets carry Opus-equivalent (weighted) tokens so the governor
        // tanks stay mix-invariant across a shifting model mix. model_mix is the
        // raw per-tier breakdown for display.
        let mut bucket_map: std::collections::BTreeMap<i64, u64> = Default::default();
        let mut mix_map: std::collections::BTreeMap<&'static str, u64> = Default::default();
        for accum in self.accums.values() {
            for e in &accum.events {
                let b = crate::governor::bucket_of(e.ts_ms);
                let weighted = (e.total as f64 * e.weight).round() as u64;
                *bucket_map.entry(b).or_insert(0) += weighted;
                *mix_map.entry(e.tier).or_insert(0) += e.total;
            }
        }
        let model_mix: Vec<(String, u64)> = mix_map
            .into_iter()
            .filter(|(_, v)| *v > 0)
            .map(|(k, v)| (k.to_string(), v))
            .collect();

        Snapshot {
            generated_at: now_ms,
            sessions: out_sessions,
            alerts,
            totals,
            usage_buckets: bucket_map.into_iter().collect(),
            rate_limits: self.rate_limit_ts(),
            limit_hits: self.limit_hits.clone(),
            weekly_usage_pct: self.weekly_usage,
            window_usage_pct: self.window_usage,
            model_mix,
            governor: None,
            pacing: None,
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
                            // Tier from THIS message's model (accum.model was
                            // just set from it; falls back to the last-known
                            // model only when this message omits one).
                            let tier = crate::config::tier_of(accum.model.as_deref());
                            accum.events.push_back(TokenEvent {
                                ts_ms: ts,
                                total: usage.billable(),
                                work: usage.input + usage.output,
                                input: usage.input,
                                cache_read: usage.cache_read,
                                tier,
                                weight: self.config.weight_for(tier),
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
                        accum.pending_tools.remove(&tool_use_id);
                    }
                    TranscriptEvent::Title { text, custom } => {
                        if custom {
                            accum.custom_title = Some(text);
                        } else {
                            accum.ai_title = Some(text);
                        }
                    }
                    TranscriptEvent::ToolStart { id, name, detail, ts_ms } => {
                        if let Some(ts) = ts_ms {
                            accum.pending_tools.insert(id, (name, detail, ts));
                        }
                    }
                    TranscriptEvent::RateLimited { ts_ms } => {
                        if let Some(ts) = ts_ms {
                            self.rate_limits.push_back(ts);
                        }
                    }
                    TranscriptEvent::LimitMarker { weekly, reset_text, ts_ms } => {
                        if let Some(ts) = ts_ms {
                            let reset_ms = crate::governor::parse_reset(&reset_text, ts);
                            let hit = crate::model::LimitHit { weekly, at_ms: ts, reset_ms };
                            if !self.limit_hits.contains(&hit) {
                                self.limit_hits.push(hit);
                            }
                        }
                    }
                    TranscriptEvent::UsageReport { pct, weekly, ts_ms } => {
                        if let Some(ts) = ts_ms {
                            let slot =
                                if weekly { &mut self.weekly_usage } else { &mut self.window_usage };
                            if slot.map(|u| ts >= u.at_ms).unwrap_or(true) {
                                *slot = Some(crate::model::UsagePct { pct, at_ms: ts });
                            }
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

    /// Find every `<project>/<sid>/subagents/` dir across all project dirs,
    /// returning `(sid, subagents_dir)`. The session id is the directory name
    /// that holds `subagents/`, matching the id used everywhere else.
    fn discover_subagent_dirs(&self) -> Vec<(String, PathBuf)> {
        // Only recently-modified subagent dirs — a global scan of every session
        // ever would resurrect thousands of historical subagents into memory and
        // bloat every snapshot. Freshness uses the real wall clock (file mtimes
        // are wall-clock), independent of the engine's simulated `now_ms`.
        let retain_secs = self.config.retention_secs() as u64;
        let mut out = Vec::new();
        let Ok(projects) = std::fs::read_dir(self.paths.projects()) else {
            return out;
        };
        for proj in projects.flatten() {
            let Ok(session_dirs) = std::fs::read_dir(proj.path()) else {
                continue;
            };
            for sd in session_dirs.flatten() {
                let sub = sd.path().join("subagents");
                let recent = std::fs::metadata(&sub)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age.as_secs() <= retain_secs)
                    .unwrap_or(false);
                if recent {
                    if let Some(sid) = sd.path().file_name().and_then(|n| n.to_str()) {
                        out.push((sid.to_string(), sub));
                    }
                }
            }
        }
        out
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

impl Engine {
    /// Ingest every subagent sidechain of `sid`: tokens roll up into both the
    /// agent's own ledger and the parent session (they drain the same tank).
    /// Ingest one session's `subagents/` directory (`<project>/<sid>/subagents`).
    /// Called for every such dir discovered across all project dirs, so it works
    /// for git-worktree sessions whose subagents live apart from the transcript.
    fn ingest_agent_sidechains(&mut self, sid: &str, dir: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // meta.json links the file to its launching tool_use and describes
            // the subagent. Retry while unresolved: the sidechain often appears
            // a beat before its meta.json is written, and caching that miss
            // would drop the subagent's tokens forever.
            let meta = match self.agent_meta.get(&path) {
                Some(Some(m)) => Some(m.clone()),
                _ => {
                    let resolved = std::fs::read_to_string(path.with_extension("meta.json"))
                        .ok()
                        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                        .and_then(|v| {
                            let tool_use_id =
                                v.get("toolUseId").and_then(|x| x.as_str())?.to_string();
                            Some(SidechainMeta {
                                tool_use_id,
                                agent_type: v
                                    .get("agentType")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("subagent")
                                    .to_string(),
                                description: v
                                    .get("description")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                            })
                        });
                    self.agent_meta.insert(path.clone(), resolved.clone());
                    resolved
                }
            };
            let Some(meta) = meta else {
                continue;
            };
            let tool_use_id = meta.tool_use_id.clone();
            self.accums
                .entry(sid.to_string())
                .or_default()
                .ensure_agent(&tool_use_id, &meta.agent_type, &meta.description);
            self.ingest_sidechain_events(sid, &path, &tool_use_id);
        }
        // Workflow fleets live one level deeper and have no launching tool call.
        self.ingest_workflow_dirs(sid, dir);
    }

    /// Read new lines from one subagent sidechain and fold them into the session
    /// accum under `agent_id`: its own ledger/rate/activity, plus a rollup into
    /// the session (subagent burn drains the same account, and keeps the session
    /// reading as active while its fleet works).
    fn ingest_sidechain_events(&mut self, sid: &str, path: &std::path::Path, agent_id: &str) {
        let watermark = *self.watermarks.get(path).unwrap_or(&0);
        let Some((lines, new_watermark)) = read_new_lines(path, watermark) else {
            return;
        };
        self.watermarks.insert(path.to_path_buf(), new_watermark);
        if lines.is_empty() {
            return;
        }
        let accum = self.accums.entry(sid.to_string()).or_default();
        for line in lines {
            for ev in transcripts::parse_line(&line) {
                match ev {
                    TranscriptEvent::Assistant { ts_ms, model, usage, .. } => {
                        let data = accum.agent_data.entry(agent_id.to_string()).or_default();
                        data.ledger.add(&usage);
                        if let Some(m) = model {
                            if !m.starts_with('<') {
                                data.model = Some(m);
                            }
                        }
                        if let Some(ts) = ts_ms {
                            data.last_activity = Some(ts.max(data.last_activity.unwrap_or(0)));
                            let tier = crate::config::tier_of(data.model.as_deref());
                            let te = TokenEvent {
                                ts_ms: ts,
                                total: usage.billable(),
                                work: usage.input + usage.output,
                                input: usage.input,
                                cache_read: usage.cache_read,
                                tier,
                                weight: self.config.weight_for(tier),
                            };
                            data.events.push_back(te);
                            accum.ledger.add(&usage);
                            accum.events.push_back(te);
                            accum.last_activity = Some(ts.max(accum.last_activity.unwrap_or(0)));
                        }
                    }
                    TranscriptEvent::ToolStart { id, name, detail, ts_ms: Some(ts) } => {
                        accum
                            .agent_data
                            .entry(agent_id.to_string())
                            .or_default()
                            .pending
                            .insert(id, (name, detail, ts));
                    }
                    TranscriptEvent::ToolResult { tool_use_id: rid, .. } => {
                        if let Some(data) = accum.agent_data.get_mut(agent_id) {
                            data.pending.remove(&rid);
                        }
                    }
                    TranscriptEvent::RateLimited { ts_ms: Some(ts) } => {
                        self.rate_limits.push_back(ts);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Discover and ingest Workflow fleets: `<subagents>/workflows/wf_*/agent-*.jsonl`.
    /// Each run becomes one synthetic `Workflow` node with its agents nested,
    /// their burn rolled into the session. Without this, a workflow's agents are
    /// invisible and a session that's only running one reads as idle.
    fn ingest_workflow_dirs(&mut self, sid: &str, subagents_dir: &std::path::Path) {
        let wf_root = subagents_dir.join("workflows");
        let Ok(dirs) = std::fs::read_dir(&wf_root) else {
            return;
        };
        let retain_secs = self.config.retention_secs() as u64;
        for d in dirs.flatten() {
            let wf_dir = d.path();
            if !wf_dir.is_dir() {
                continue;
            }
            let recent = std::fs::metadata(&wf_dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age.as_secs() <= retain_secs)
                .unwrap_or(false);
            if !recent {
                continue;
            }
            let Some(wf_id) = wf_dir.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };
            let label = self.workflow_label(subagents_dir, &wf_id);
            let Ok(files) = std::fs::read_dir(&wf_dir) else {
                continue;
            };
            let mut children: Vec<(String, String)> = Vec::new();
            for f in files.flatten() {
                let p = f.path();
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !stem.starts_with("agent-")
                    || p.extension().and_then(|e| e.to_str()) != Some("jsonl")
                {
                    continue;
                }
                let child_id = format!("{wf_id}/{stem}");
                let desc = self.workflow_child_desc(&p);
                self.ingest_sidechain_events(sid, &p, &child_id);
                children.push((child_id, desc));
            }
            if children.is_empty() {
                continue;
            }
            self.accums
                .entry(sid.to_string())
                .or_default()
                .ensure_workflow(&wf_id, &label, &children);
        }
    }

    /// The workflow's human name, from its persisted script filename
    /// `workflows/scripts/<name>-<wf_id>.js`. Falls back to "workflow".
    fn workflow_label(&self, subagents_dir: &std::path::Path, wf_id: &str) -> String {
        let Some(parent) = subagents_dir.parent() else {
            return "workflow".to_string();
        };
        let suffix = format!("-{wf_id}.js");
        if let Ok(rd) = std::fs::read_dir(parent.join("workflows").join("scripts")) {
            for e in rd.flatten() {
                if let Some(stripped) = e
                    .file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(&suffix))
                {
                    return stripped.to_string();
                }
            }
        }
        "workflow".to_string()
    }

    /// A short label for one workflow agent — the opening line of its task
    /// prompt. Resolved once per file and cached.
    fn workflow_child_desc(&mut self, path: &std::path::Path) -> String {
        if let Some(d) = self.wf_child_desc.get(path) {
            return d.clone();
        }
        let desc = first_prompt_summary(path);
        self.wf_child_desc.insert(path.to_path_buf(), desc.clone());
        desc
    }
}

/// First line of a subagent transcript → a short one-line task summary.
fn first_prompt_summary(path: &std::path::Path) -> String {
    use std::io::BufRead;
    let fallback = || "agent".to_string();
    let Ok(file) = std::fs::File::open(path) else {
        return fallback();
    };
    let mut first = String::new();
    if std::io::BufReader::new(file).read_line(&mut first).is_err() || first.is_empty() {
        return fallback();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&first) else {
        return fallback();
    };
    let text = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.is_empty() {
        return fallback();
    }
    let mut s: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        s.push('…');
    }
    s
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
