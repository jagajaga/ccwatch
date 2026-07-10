//! TUI application state and the pure logic over it (navigation, expansion,
//! fuzzy jump, action staging). Rendering lives in `ui`; daemon I/O in `client`.

use ccwatch_core::ipc::ActionRequest;
use ccwatch_core::model::{Agent, Host, Session, Snapshot};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::collections::HashSet;

/// A reference to a row in the session/agent tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowRef {
    Session(usize),
    /// (session index, path of child indices into the agent tree).
    Agent(usize, Vec<usize>),
}

#[derive(Clone, Debug)]
pub struct VisibleRow {
    pub row: RowRef,
    /// Stable key: session id or agent id.
    pub key: String,
    pub depth: usize,
}

/// A fuzzy-jump candidate spanning every entity type.
#[derive(Clone, Debug)]
pub struct JumpItem {
    pub label: String,
    pub kind: &'static str,
    pub session_id: String,
    /// Chain of agent ids from the top-level agent down to this item, so we can
    /// expand ancestors before selecting.
    pub agent_path: Vec<String>,
}

/// Session-list sort order, cycled with `s`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Burn,
    LastActive,
    Name,
    Cpu,
    Rss,
}

impl SortBy {
    pub fn next(self) -> Self {
        match self {
            SortBy::Burn => SortBy::LastActive,
            SortBy::LastActive => SortBy::Name,
            SortBy::Name => SortBy::Cpu,
            SortBy::Cpu => SortBy::Rss,
            SortBy::Rss => SortBy::Burn,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SortBy::Burn => "tok/min",
            SortBy::LastActive => "last active",
            SortBy::Name => "name",
            SortBy::Cpu => "cpu",
            SortBy::Rss => "rss",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PendingAction {
    pub request: ActionRequest,
    pub prompt: String,
}

pub enum Mode {
    Normal,
    Fuzzy {
        query: String,
        results: Vec<JumpItem>,
        cursor: usize,
    },
    Confirm(PendingAction),
    /// Full-detail popup for the selected row.
    Details,
}

pub struct App {
    pub snapshot: Snapshot,
    pub connected: bool,
    pub status: Option<String>,
    pub expanded: HashSet<String>,
    pub selected: usize,
    pub hide_idle: bool,
    /// Hide finished agents from the tree (history vs. live view).
    pub hide_done: bool,
    pub sort: SortBy,
    pub mode: Mode,
    pub should_quit: bool,
    pub now_ms: i64,
    matcher: SkimMatcherV2,
    /// After a jump, the key we want selected once rows are recomputed.
    pending_select: Option<String>,
}

impl App {
    pub fn new(now_ms: i64) -> Self {
        App {
            snapshot: Snapshot::empty(now_ms),
            connected: false,
            status: None,
            expanded: HashSet::new(),
            selected: 0,
            hide_idle: false,
            hide_done: false,
            sort: SortBy::Burn,
            mode: Mode::Normal,
            should_quit: false,
            now_ms,
            matcher: SkimMatcherV2::default(),
            pending_select: None,
        }
    }

    pub fn set_snapshot(&mut self, snap: Snapshot) {
        self.now_ms = snap.generated_at;
        self.snapshot = snap;
        self.reconcile_selection();
    }

    /// Sessions honoring the idle filter, ordered by the active sort.
    pub fn sessions(&self) -> Vec<(usize, &Session)> {
        let mut out: Vec<(usize, &Session)> = self
            .snapshot
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                // "Hide idle" must never hide a session that's still burning —
                // long tool calls emit no messages and look idle while working.
                !self.hide_idle
                    || !matches!(s.state, ccwatch_core::model::SessionState::Idle)
                    || s.tokens_per_min >= 1.0
            })
            .collect();
        match self.sort {
            SortBy::Burn => out.sort_by(|a, b| {
                b.1.tokens_per_min
                    .partial_cmp(&a.1.tokens_per_min)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortBy::LastActive => {
                out.sort_by_key(|(_, s)| std::cmp::Reverse(s.last_activity.unwrap_or(0)))
            }
            SortBy::Name => out.sort_by(|a, b| a.1.name.cmp(&b.1.name)),
            SortBy::Cpu => out.sort_by(|a, b| {
                b.1.cpu_pct
                    .partial_cmp(&a.1.cpu_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortBy::Rss => out.sort_by_key(|(_, s)| std::cmp::Reverse(s.rss_mb)),
        }
        out
    }

    /// Flatten sessions and (expanded) agent trees into display rows.
    pub fn visible_rows(&self) -> Vec<VisibleRow> {
        let mut rows = Vec::new();
        for (si, s) in self.sessions() {
            rows.push(VisibleRow {
                row: RowRef::Session(si),
                key: s.id.clone(),
                depth: 0,
            });
            if self.expanded.contains(&s.id) {
                push_agents(
                    &s.agents,
                    si,
                    &mut Vec::new(),
                    1,
                    &self.expanded,
                    self.hide_done,
                    &mut rows,
                );
            }
        }
        rows
    }

    fn reconcile_selection(&mut self) {
        let rows = self.visible_rows();
        if let Some(key) = self.pending_select.take() {
            if let Some(idx) = rows.iter().position(|r| r.key == key) {
                self.selected = idx;
                return;
            }
        }
        if rows.is_empty() {
            self.selected = 0;
        } else if self.selected >= rows.len() {
            self.selected = rows.len() - 1;
        }
    }

    pub fn selected_row(&self) -> Option<VisibleRow> {
        self.visible_rows().into_iter().nth(self.selected)
    }

    /// The session that owns the current selection (session row or agent row).
    pub fn selected_session(&self) -> Option<&Session> {
        match self.selected_row()?.row {
            RowRef::Session(i) | RowRef::Agent(i, _) => self.snapshot.sessions.get(i),
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.visible_rows().len();
        if len == 0 {
            return;
        }
        let cur = self.selected as isize;
        let next = (cur + delta).clamp(0, len as isize - 1);
        self.selected = next as usize;
    }

    /// Expand/collapse the selected row if it has children.
    pub fn toggle_expand(&mut self) {
        if let Some(vr) = self.selected_row() {
            let has_children = match &vr.row {
                RowRef::Session(i) => self
                    .snapshot
                    .sessions
                    .get(*i)
                    .map(|s| !s.agents.is_empty())
                    .unwrap_or(false),
                RowRef::Agent(i, path) => agent_at(&self.snapshot.sessions[*i].agents, path)
                    .map(|a| !a.children.is_empty())
                    .unwrap_or(false),
            };
            if has_children {
                if !self.expanded.remove(&vr.key) {
                    self.expanded.insert(vr.key);
                }
                self.status = None;
            } else {
                // Enter must never look dead: explain why nothing expanded.
                self.status = Some("no subagents recorded for this row".into());
            }
        }
    }

    // ---- fuzzy jump ---------------------------------------------------------

    pub fn open_fuzzy(&mut self) {
        self.mode = Mode::Fuzzy {
            query: String::new(),
            results: self.all_jump_items(),
            cursor: 0,
        };
    }

    pub fn fuzzy_input(&mut self, c: char) {
        if let Mode::Fuzzy { query, .. } = &mut self.mode {
            query.push(c);
            self.refresh_fuzzy();
        }
    }

    pub fn fuzzy_backspace(&mut self) {
        if let Mode::Fuzzy { query, .. } = &mut self.mode {
            query.pop();
            self.refresh_fuzzy();
        }
    }

    pub fn fuzzy_move(&mut self, delta: isize) {
        if let Mode::Fuzzy { results, cursor, .. } = &mut self.mode {
            if results.is_empty() {
                return;
            }
            let n = results.len() as isize;
            *cursor = ((*cursor as isize + delta).rem_euclid(n)) as usize;
        }
    }

    fn refresh_fuzzy(&mut self) {
        let all = self.all_jump_items();
        if let Mode::Fuzzy { query, results, cursor } = &mut self.mode {
            if query.is_empty() {
                *results = all;
            } else {
                let mut scored: Vec<(i64, JumpItem)> = all
                    .into_iter()
                    .filter_map(|it| {
                        self.matcher
                            .fuzzy_match(&it.label, query)
                            .map(|score| (score, it))
                    })
                    .collect();
                scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
                *results = scored.into_iter().map(|(_, it)| it).collect();
            }
            *cursor = 0;
        }
    }

    /// Commit the highlighted fuzzy result: expand ancestors and select it.
    pub fn fuzzy_commit(&mut self) {
        let chosen = if let Mode::Fuzzy { results, cursor, .. } = &self.mode {
            results.get(*cursor).cloned()
        } else {
            None
        };
        self.mode = Mode::Normal;
        if let Some(item) = chosen {
            self.expanded.insert(item.session_id.clone());
            for aid in &item.agent_path {
                self.expanded.insert(aid.clone());
            }
            // Select the deepest entity that maps to a row: the last agent in
            // the path, else the session.
            self.pending_select = Some(
                item.agent_path
                    .last()
                    .cloned()
                    .unwrap_or(item.session_id.clone()),
            );
            self.reconcile_selection();
        }
    }

    fn all_jump_items(&self) -> Vec<JumpItem> {
        let mut items = Vec::new();
        for s in &self.snapshot.sessions {
            items.push(JumpItem {
                label: format!(
                    "session {} {} [{}]",
                    s.title.as_deref().unwrap_or(""),
                    s.name,
                    s.host.label()
                ),
                kind: "session",
                session_id: s.id.clone(),
                agent_path: Vec::new(),
            });
            collect_agent_items(&s.agents, &s.id, &mut Vec::new(), &mut items);
            for t in &s.tasks {
                items.push(JumpItem {
                    label: format!("task {}", t.subject),
                    kind: "task",
                    session_id: s.id.clone(),
                    agent_path: Vec::new(),
                });
            }
            for w in &s.watchers {
                items.push(JumpItem {
                    label: format!("watcher {} {}", w.name, w.detail),
                    kind: "watcher",
                    session_id: s.id.clone(),
                    agent_path: Vec::new(),
                });
            }
        }
        items
    }

    // ---- actions ------------------------------------------------------------

    /// Stage a kill/pause/resume for the selected session (confirmation next).
    ///
    /// Local sessions use OS signals against the pid. Remote/cloud sessions
    /// can't be signalled from here (the pid lives on another machine), so
    /// "kill" maps to the host's configured cancel command and pause/resume are
    /// unavailable.
    pub fn stage_action(&mut self, kind: ActionKind) {
        let Some(s) = self.selected_session() else {
            return;
        };
        // Snapshot the owned fields we need, then drop the borrow of self.
        let is_local = matches!(s.host, Host::Local);
        let host_label = s.host.label();
        let pid = s.pid;
        let name = s.name.clone();
        let id = s.id.clone();
        let remote_name = s.remote_name.clone();

        if !is_local {
            match kind {
                ActionKind::Kill => {
                    let Some(remote) = remote_name else {
                        self.status = Some("remote session has no host name to target".into());
                        return;
                    };
                    self.mode = Mode::Confirm(PendingAction {
                        request: ActionRequest::CancelRemote { remote, id },
                        prompt: format!(
                            "Cancel \"{name}\" on {host_label} (runs host cancel command)? [y/n]"
                        ),
                    });
                }
                ActionKind::Pause | ActionKind::Resume => {
                    self.status = Some(
                        "pause/resume is local-only; use k to cancel a remote/cloud session".into(),
                    );
                }
            }
            return;
        }

        let Some(pid) = pid else {
            self.status = Some("selected session has no pid".into());
            return;
        };
        let (request, prompt) = match kind {
            ActionKind::Kill => (
                ActionRequest::KillSession { pid },
                format!("Kill session \"{name}\" (pid {pid})? SIGTERM→SIGKILL. [y/n]"),
            ),
            ActionKind::Pause => (
                ActionRequest::PauseSession { pid },
                format!("Pause session \"{name}\" (pid {pid})? SIGSTOP. [y/n]"),
            ),
            ActionKind::Resume => (
                ActionRequest::ResumeSession { pid },
                format!("Resume session \"{name}\" (pid {pid})? SIGCONT. [y/n]"),
            ),
        };
        self.mode = Mode::Confirm(PendingAction { request, prompt });
    }

    /// Cycle the selected session's Cruise priority override: auto → High → Low →
    /// auto. Returns `(session_id, priority)` to send (no confirm — it's a cheap,
    /// reversible pin), or `None` if no session is selected.
    pub fn cycle_session_priority(&self) -> Option<(String, String)> {
        use ccwatch_core::model::Priority;
        let s = self.selected_session()?;
        let next = match s.priority_override {
            None => "high",
            Some(Priority::High) => "low",
            Some(Priority::Low) | Some(Priority::Normal) => "normal",
        };
        Some((s.id.clone(), next.to_string()))
    }

    /// Stage the Cruise Control recommendation (pause its background sessions)
    /// for confirmation, if there's a plan with something to pause.
    pub fn stage_apply_pacing(&mut self) {
        if let Some(p) = self.snapshot.pacing.as_ref() {
            let n = p.pause_pids().len();
            if n > 0 {
                self.mode = Mode::Confirm(PendingAction {
                    request: ActionRequest::ApplyPacing,
                    prompt: format!("Pause {n} background session(s) to coast?"),
                });
            }
        }
    }

    /// Stage a release of autonomous Cruise Control (mode "off") for
    /// confirmation, if the daemon currently has it enforcing a plan.
    pub fn stage_release_cruise(&mut self) {
        if let Some(p) = self.snapshot.pacing.as_ref() {
            if p.auto {
                self.mode = Mode::Confirm(PendingAction {
                    request: ActionRequest::SetCruiseMode { mode: "off".into() },
                    prompt: format!("Release Cruise auto ({} paused)? Resumes everything.", p.paced),
                });
            }
        }
    }

    /// Take the staged request (on confirm).
    pub fn take_pending(&mut self) -> Option<ActionRequest> {
        if let Mode::Confirm(p) = &self.mode {
            let req = p.request.clone();
            self.mode = Mode::Normal;
            Some(req)
        } else {
            None
        }
    }

    pub fn cancel_mode(&mut self) {
        self.mode = Mode::Normal;
    }
}

#[derive(Clone, Copy)]
pub enum ActionKind {
    Kill,
    Pause,
    Resume,
}

fn push_agents(
    agents: &[Agent],
    si: usize,
    path: &mut Vec<usize>,
    depth: usize,
    expanded: &HashSet<String>,
    hide_done: bool,
    out: &mut Vec<VisibleRow>,
) {
    use ccwatch_core::model::AgentState;
    for (ai, a) in agents.iter().enumerate() {
        if hide_done && matches!(a.state, AgentState::Finished) {
            continue;
        }
        path.push(ai);
        out.push(VisibleRow {
            row: RowRef::Agent(si, path.clone()),
            key: a.id.clone(),
            depth,
        });
        if expanded.contains(&a.id) && !a.children.is_empty() {
            push_agents(&a.children, si, path, depth + 1, expanded, hide_done, out);
        }
        path.pop();
    }
}

fn collect_agent_items(
    agents: &[Agent],
    session_id: &str,
    ancestors: &mut Vec<String>,
    out: &mut Vec<JumpItem>,
) {
    for a in agents {
        ancestors.push(a.id.clone());
        out.push(JumpItem {
            label: format!("agent {} [{}]", a.description, a.subagent_type),
            kind: "agent",
            session_id: session_id.to_string(),
            agent_path: ancestors.clone(),
        });
        collect_agent_items(&a.children, session_id, ancestors, out);
        ancestors.pop();
    }
}

/// Resolve an agent by child-index path.
pub fn agent_at<'a>(agents: &'a [Agent], path: &[usize]) -> Option<&'a Agent> {
    let mut cur = agents;
    let mut node = None;
    for &i in path {
        node = cur.get(i);
        cur = &node?.children;
    }
    node
}

#[cfg(test)]
pub(crate) mod test_support {
    use ccwatch_core::model::*;

    pub fn agent(id: &str, desc: &str, children: Vec<Agent>) -> Agent {
        Agent {
            id: id.into(),
            subagent_type: "general-purpose".into(),
            description: desc.into(),
            model: Some("opus".into()),
            state: AgentState::Running,
            started_at: Some(1000),
            tokens: TokenLedger::default(),
            tokens_per_min: 0.0,
            activity: vec![],
            last_activity: None,
            children,
        }
    }

    pub fn session(id: &str, name: &str, agents: Vec<Agent>) -> Session {
        Session {
            id: id.into(),
            name: name.into(),
            title: None,
            cwd: "/tmp/proj".into(),
            pid: Some(4242),
            kind: "interactive".into(),
            entrypoint: "cli".into(),
            version: "2.1.0".into(),
            model: Some("claude-opus-4-8".into()),
            state: SessionState::Running,
            started_at: Some(0),
            last_activity: Some(9_000),
            last_user_turn: Some(9_000),
            priority_override: None,
            tokens: TokenLedger {
                input: 4000,
                output: 8000,
                cache_write: 2000,
                cache_read: 180_000,
                messages: 12,
                ..Default::default()
            },
            tokens_per_min: 62_000.0,
            cpu_pct: 34.0,
            rss_mb: 420,
            agents,
            tasks: vec![Task {
                subject: "do the thing".into(),
                status: "in_progress".into(),
                blocked: false,
                active_form: None,
            }],
            watchers: vec![Watcher {
                kind: WatcherKind::Loop,
                name: "ScheduleWakeup".into(),
                detail: "babysit".into(),
                schedule: Some("self-paced".into()),
                last_fired: Some(5000),
                fired_count: 8,
                next_wake: Some(20_000),
                running: true,
                pid: None,
            }],
            activity: vec![ccwatch_core::model::Activity {
                tool: "Edit".into(),
                detail: "core/src/engine.rs".into(),
                since_ms: 5_000,
            }],
            processes: vec![ccwatch_core::model::ProcInfo {
                pid: 777,
                name: "cargo".into(),
                cmd: "cargo build --release".into(),
                cpu_pct: 87.0,
                rss_mb: 512,
                run_secs: 192,
            }],
            host: Host::Local,
            remote_name: None,
        }
    }

    pub fn snapshot(sessions: Vec<Session>) -> Snapshot {
        let active = sessions.len();
        Snapshot {
            generated_at: 10_000,
            sessions,
            alerts: vec![Alert {
                severity: Severity::Critical,
                kind: AlertKind::RunawayLoop,
                subject: "webapp".into(),
                session_id: "s1".into(),
                message: "62k tok/min · no user turn 7m".into(),
                since_ms: 0,
            }],
            totals: Totals {
                active_sessions: active,
                tokens_per_min: 62_000.0,
                total_tokens: 194_000,
                cache_hit_pct: 92.0,
            },
            usage_buckets: Vec::new(),
            rate_limits: Vec::new(),
            limit_hits: Vec::new(),
            model_mix: Vec::new(),
            weekly_usage_pct: None,
            window_usage_pct: None,
            governor: None,
            pacing: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use ccwatch_core::model::SessionState;

    fn app_with(snap: Snapshot) -> App {
        let mut a = App::new(10_000);
        a.set_snapshot(snap);
        a
    }

    #[test]
    fn navigation_and_expand_reveal_agents() {
        let snap = snapshot(vec![session(
            "s1",
            "webapp",
            vec![agent("a1", "search dir", vec![agent("a2", "sub-scan", vec![])])],
        )]);
        let mut app = app_with(snap);
        // Only the session row is visible initially.
        assert_eq!(app.visible_rows().len(), 1);
        // Expand the session → its agent appears.
        app.toggle_expand();
        assert_eq!(app.visible_rows().len(), 2);
        // Move to the agent and expand → nested child appears.
        app.move_selection(1);
        app.toggle_expand();
        assert_eq!(app.visible_rows().len(), 3);
        assert_eq!(app.visible_rows()[2].key, "a2");
    }

    #[test]
    fn fuzzy_matches_across_entities_and_jumps() {
        let snap = snapshot(vec![session(
            "s1",
            "webapp",
            vec![agent("a1", "search dir", vec![])],
        )]);
        let mut app = app_with(snap);
        app.open_fuzzy();
        for c in "search".chars() {
            app.fuzzy_input(c);
        }
        // Top result should be the agent whose description matches.
        if let Mode::Fuzzy { results, .. } = &app.mode {
            assert!(results.iter().any(|r| r.kind == "agent"));
            assert_eq!(results[0].kind, "agent");
        } else {
            panic!("not in fuzzy mode");
        }
        app.fuzzy_commit();
        // Jump expanded the session and selected the agent row.
        assert!(app.expanded.contains("s1"));
        assert_eq!(app.selected_row().unwrap().key, "a1");
    }

    #[test]
    fn stage_kill_enters_confirm_with_pid() {
        let snap = snapshot(vec![session("s1", "webapp", vec![])]);
        let mut app = app_with(snap);
        app.stage_action(ActionKind::Kill);
        match app.take_pending() {
            Some(ccwatch_core::ipc::ActionRequest::KillSession { pid }) => assert_eq!(pid, 4242),
            other => panic!("expected KillSession, got {other:?}"),
        }
    }

    #[test]
    fn apply_cruise_key_stages_apply_pacing_confirm() {
        use ccwatch_core::model::{PaceAction, PacingPlan};
        let mut snap = snapshot(vec![session("s1", "webapp", vec![])]);
        snap.pacing = Some(PacingPlan {
            target_rate: 100_000.0,
            actual_rate: 500_000.0,
            price: 1e-5,
            actions: vec![PaceAction::Pause {
                pid: 7,
                ssh: None,
                reason: "x".into(),
            }],
            reason: "over".into(),
            auto: false,
            paced: 0,
        });
        let mut app = app_with(snap);
        app.stage_apply_pacing();
        match app.take_pending() {
            Some(ccwatch_core::ipc::ActionRequest::ApplyPacing) => {}
            other => panic!("expected ApplyPacing confirm, got {other:?}"),
        }
    }

    #[test]
    fn release_cruise_key_stages_set_cruise_mode_off_confirm_when_auto() {
        use ccwatch_core::model::PacingPlan;
        let mut snap = snapshot(vec![session("s1", "webapp", vec![])]);
        snap.pacing = Some(PacingPlan {
            target_rate: 100_000.0,
            actual_rate: 500_000.0,
            price: 1e-5,
            actions: vec![],
            reason: "auto-pacing".into(),
            auto: true,
            paced: 3,
        });
        let mut app = app_with(snap);
        app.stage_release_cruise();
        match app.take_pending() {
            Some(ccwatch_core::ipc::ActionRequest::SetCruiseMode { mode }) => {
                assert_eq!(mode, "off")
            }
            other => panic!("expected SetCruiseMode {{ mode: \"off\" }}, got {other:?}"),
        }
    }

    #[test]
    fn release_cruise_key_noop_when_not_auto() {
        // Not in auto mode (or no plan at all): the release key must not stage
        // anything — there's nothing Cruise-paced to release.
        let snap = snapshot(vec![session("s1", "webapp", vec![])]);
        let mut app = app_with(snap);
        app.stage_release_cruise();
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn remote_kill_maps_to_cancel_and_pause_is_blocked() {
        let mut s = session("s1", "worker", vec![]);
        s.host = Host::Remote {
            name: "demo-host".into(),
            ssh_target: "user@demo-host".into(),
        };
        s.remote_name = Some("demo-host".into());
        let mut app = app_with(snapshot(vec![s]));

        // Kill on a remote session becomes a CancelRemote targeting its host.
        app.stage_action(ActionKind::Kill);
        match app.take_pending() {
            Some(ccwatch_core::ipc::ActionRequest::CancelRemote { remote, id }) => {
                assert_eq!(remote, "demo-host");
                assert_eq!(id, "s1");
            }
            other => panic!("expected CancelRemote, got {other:?}"),
        }

        // Pause is local-only: no confirm staged, a status message instead.
        app.stage_action(ActionKind::Pause);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_deref().unwrap_or("").contains("local-only"));
    }

    #[test]
    fn sort_cycles_and_reorders() {
        let mut a = session("s1", "zebra", vec![]);
        a.tokens_per_min = 50_000.0;
        a.last_activity = Some(1_000);
        let mut b = session("s2", "alpha", vec![]);
        b.tokens_per_min = 1_000.0;
        b.last_activity = Some(9_000);
        let mut app = app_with(snapshot(vec![a, b]));

        // Default: burn desc → zebra first.
        assert_eq!(app.sessions()[0].1.name, "zebra");
        // Cycle to last-active → alpha (more recent) first.
        app.sort = app.sort.next();
        assert_eq!(app.sort, SortBy::LastActive);
        assert_eq!(app.sessions()[0].1.name, "alpha");
        // Cycle to name → alpha first alphabetically.
        app.sort = app.sort.next();
        assert_eq!(app.sessions()[0].1.name, "alpha");
        // Full cycle returns to burn.
        app.sort = app.sort.next().next().next();
        assert_eq!(app.sort, SortBy::Burn);
    }

    #[test]
    fn hide_done_filters_finished_agents() {
        let mut done = agent("a-done", "finished work", vec![]);
        done.state = ccwatch_core::model::AgentState::Finished;
        let running = agent("a-run", "live work", vec![]);
        let snap = snapshot(vec![session("s1", "busy", vec![done, running])]);
        let mut app = app_with(snap);
        app.toggle_expand(); // expand the session
        assert_eq!(app.visible_rows().len(), 3, "session + 2 agents");
        app.hide_done = true;
        let rows = app.visible_rows();
        assert_eq!(rows.len(), 2, "session + only the running agent");
        assert_eq!(rows[1].key, "a-run");
    }

    #[test]
    fn hide_idle_filters_sessions_but_keeps_burning_ones() {
        let mut quiet = session("s2", "sleepy", vec![]);
        quiet.state = SessionState::Idle;
        quiet.tokens_per_min = 0.0;
        // Idle by message-timestamps but still burning (long tool call).
        let mut grinding = session("s3", "grinding", vec![]);
        grinding.state = SessionState::Idle;
        grinding.tokens_per_min = 5_000.0;
        let snap = snapshot(vec![session("s1", "busy", vec![]), quiet, grinding]);
        let mut app = app_with(snap);
        assert_eq!(app.sessions().len(), 3);
        app.hide_idle = true;
        let names: Vec<&str> = app.sessions().iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(names, vec!["busy", "grinding"], "burning session must survive the filter");
    }
}
