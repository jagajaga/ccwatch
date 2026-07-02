//! Rendering. A pure function of [`App`] → frame; no state mutation here.

use crate::app::{agent_at, App, Mode, RowRef};
use crate::format;
use ccwatch_core::model::{AgentState, Alert, Host, Session, SessionState, Severity, WatcherKind};
use std::collections::BTreeMap;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

/// tok/min at which the burn column turns red (matches the daemon's default
/// `burn_tokens_per_min`; purely cosmetic here).
const BURN_RED: f64 = 40_000.0;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let alert_h = (app.snapshot.alerts.len() as u16).clamp(1, 4) + 2;
    let chunks = Layout::vertical([
        Constraint::Length(1),        // top bar
        Constraint::Length(alert_h),  // alerts
        Constraint::Min(5),           // tree
        Constraint::Length(8),        // tasks | watchers
        Constraint::Length(6),        // details
        Constraint::Length(1),        // footer
    ])
    .split(area);

    draw_topbar(f, chunks[0], app);
    draw_alerts(f, chunks[1], app);
    draw_tree(f, chunks[2], app);
    draw_bottom_split(f, chunks[3], app);
    draw_details(f, chunks[4], app);
    draw_footer(f, chunks[5], app);

    match &app.mode {
        Mode::Fuzzy { query, results, cursor } => draw_fuzzy(f, area, query, results, *cursor),
        Mode::Confirm(p) => draw_confirm(f, area, &p.prompt),
        Mode::Details => draw_details_popup(f, area, app),
        Mode::Normal => {}
    }
}

/// Governor segment for the top bar: "▲2.1× wall 49m" / "▼0.6×".
fn governor_segment(app: &App) -> String {
    let Some(g) = &app.snapshot.governor else {
        return String::new();
    };
    let w = &g.window;
    let Some(delta) = g.primary_delta() else {
        return String::new();
    };
    let arrow = if delta.is_infinite() || delta >= ccwatch_core::governor::DELTA_EMPTY {
        return " · ⛔ tank empty".to_string();
    } else if delta >= 1.0 {
        format!("▲{delta:.1}×")
    } else {
        format!("▼{delta:.1}×")
    };
    match w.wall_at {
        Some(wall) => format!(
            " · {arrow} wall {}",
            format::duration_ms(wall - app.now_ms)
        ),
        None => format!(" · {arrow}"),
    }
}

fn draw_topbar(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.snapshot.totals;
    let conn = if app.connected { "●" } else { "○ disconnected" };
    // Fleet-wide agent census: running/total across every session.
    let (mut running, mut total) = (0usize, 0usize);
    fn census(agents: &[ccwatch_core::model::Agent], r: &mut usize, t: &mut usize) {
        for a in agents {
            *t += 1;
            if matches!(a.state, AgentState::Running) {
                *r += 1;
            }
            census(&a.children, r, t);
        }
    }
    for s in &app.snapshot.sessions {
        census(&s.agents, &mut running, &mut total);
    }
    let agents_seg = if total > 0 {
        format!(" · ⚒ {running}▶/{total} agents")
    } else {
        String::new()
    };
    let text = format!(
        " ccwatch  {conn}  {} · {} active · {} tok/min · Σ {} · cache {:.0}%{}{}",
        host_breakdown(app),
        t.active_sessions,
        format::rate(t.tokens_per_min),
        format::tokens(t.total_tokens),
        t.cache_hit_pct,
        agents_seg,
        governor_segment(app),
    );
    let style = if app.connected {
        Style::default().fg(Color::Black).bg(Color::Cyan)
    } else {
        Style::default().fg(Color::White).bg(Color::Red)
    };
    f.render_widget(
        Paragraph::new(text).style(style.add_modifier(Modifier::BOLD)),
        area,
    );
}

/// "local 3" or "local 3 · demo-host 1 · cloud 2" across every host present.
fn host_breakdown(app: &App) -> String {
    let mut counts: BTreeMap<(u8, String), usize> = BTreeMap::new();
    for s in &app.snapshot.sessions {
        let rank = match s.host {
            Host::Local => 0u8,
            Host::Remote { .. } => 1,
            Host::Cloud => 2,
        };
        *counts.entry((rank, s.host.label())).or_default() += 1;
    }
    if counts.is_empty() {
        return "local".to_string();
    }
    counts
        .into_iter()
        .map(|((_, label), n)| format!("{label} {n}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn draw_alerts(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" ALERTS ({}) ", app.snapshot.alerts.len()));
    if app.snapshot.alerts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no leaks detected",
            Style::default().fg(Color::DarkGray),
        )))
        .block(block);
        f.render_widget(p, area);
        return;
    }
    let items: Vec<ListItem> = app
        .snapshot
        .alerts
        .iter()
        .map(|a| ListItem::new(alert_line(a)))
        .collect();
    f.render_widget(List::new(items).block(block), area);
}

fn alert_line(a: &Alert) -> Line<'static> {
    let color = match a.severity {
        Severity::Critical => Color::Red,
        Severity::Warn => Color::Yellow,
    };
    Line::from(vec![
        Span::styled("⚠ ", Style::default().fg(color)),
        Span::styled(
            format!("{:<14}", a.kind.label()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{:<22}", a.subject), Style::default().fg(Color::White)),
        Span::raw(a.message.clone()),
    ])
}

/// Column widths for the session/agent grid. Base values fit a ~110-col
/// terminal; anything wider is distributed to the data-dense columns
/// (breakdown, tok/min, then name/host).
#[derive(Clone, Copy)]
struct Widths {
    name: usize, // includes the 2-char expand marker
    host: usize,
    state: usize,
    up: usize,
    last: usize,
    tpm: usize,
    breakdown: usize,
    cpu: usize,
    rss: usize,
}

impl Widths {
    fn for_width(inner_width: usize) -> Self {
        let mut w = Widths {
            name: 24,
            host: 12,
            state: 8,
            up: 7,
            last: 7,
            tpm: 8,
            breakdown: 27,
            cpu: 5,
            rss: 6,
        };
        const MODEL_RESERVE: usize = 12;
        let base = w.name
            + w.host
            + w.state
            + w.up
            + w.last
            + w.tpm
            + w.breakdown
            + w.cpu
            + w.rss;
        let mut extra = inner_width.saturating_sub(base + MODEL_RESERVE);
        let mut grow = |slot: &mut usize, by: usize| {
            let g = by.min(extra);
            *slot += g;
            extra -= g;
        };
        grow(&mut w.breakdown, 12);
        grow(&mut w.tpm, 4);
        grow(&mut w.name, 14);
        grow(&mut w.host, 8);
        w
    }
}

/// Truncate-and-pad to an exact width (left-aligned).
fn cell(s: &str, w: usize) -> String {
    format!("{:<w$}", truncate(s, w.saturating_sub(1)), w = w)
}

/// Truncate-and-pad to an exact width (right-aligned, one trailing space).
fn cell_r(s: &str, w: usize) -> String {
    format!("{:>w$} ", truncate(s, w.saturating_sub(1)), w = w.saturating_sub(1))
}

/// Row-selection highlight: a background tint that keeps each cell's own
/// color readable (REVERSED turned colored cells into a patchwork).
fn selection_style() -> Style {
    Style::default().bg(Color::Rgb(36, 52, 84))
}

fn header_line(w: &Widths, sort: crate::app::SortBy) -> Line<'static> {
    use crate::app::SortBy;
    // The active sort column gets a ▾ marker.
    let mark = |label: &str, this: SortBy| {
        if sort == this {
            format!("{label}▾")
        } else {
            label.to_string()
        }
    };
    let text = format!(
        "{}{}{}{}{}{}{}{}{}model",
        cell(&format!("  {}", mark("name/desc", SortBy::Name)), w.name),
        cell("host", w.host),
        cell("state", w.state),
        cell("up", w.up),
        cell(&mark("last", SortBy::LastActive), w.last),
        cell_r(&mark("tok/min", SortBy::Burn), w.tpm),
        cell("in/out/cw/cr", w.breakdown),
        cell_r(&mark("cpu", SortBy::Cpu), w.cpu),
        cell_r(&mark("rss", SortBy::Rss), w.rss),
    );
    Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))
}

fn draw_tree(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.visible_rows();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" SESSIONS / AGENTS ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let w = Widths::for_width(inner.width as usize);
    let chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    // Header row rendered by the same cell code as the data → exact alignment.
    f.render_widget(Paragraph::new(header_line(&w, app.sort)), chunks[0]);

    let list_h = chunks[1].height as usize;
    let start = if app.selected >= list_h {
        app.selected - list_h + 1
    } else {
        0
    };

    let mut items = Vec::new();
    for (i, vr) in rows.iter().enumerate().skip(start).take(list_h) {
        let selected = i == app.selected;
        let line = match &vr.row {
            RowRef::Session(si) => session_line(&app.snapshot.sessions[*si], app, &w),
            RowRef::Agent(si, path) => {
                let a = agent_at(&app.snapshot.sessions[*si].agents, path);
                agent_line(a, vr.depth, app, &w)
            }
        };
        let item = if selected {
            ListItem::new(line).style(selection_style())
        } else {
            ListItem::new(line)
        };
        items.push(item);
    }
    if items.is_empty() {
        items.push(ListItem::new(Span::styled(
            "no active sessions",
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(List::new(items), chunks[1]);
}

fn state_span(state: SessionState, w: &Widths) -> Span<'static> {
    let (txt, color) = match state {
        SessionState::Running => ("running", Color::Green),
        SessionState::Waiting => ("waiting", Color::Cyan),
        SessionState::Idle => ("idle", Color::Gray),
        SessionState::Ended => ("ended", Color::Gray),
    };
    Span::styled(cell(txt, w.state), Style::default().fg(color))
}

fn burn_span(tpm: f64, threshold: f64, w: &Widths) -> Span<'static> {
    let color = if tpm >= threshold {
        Color::Red
    } else if tpm >= threshold / 2.0 {
        Color::Yellow
    } else {
        Color::Gray
    };
    Span::styled(cell_r(&format::rate(tpm), w.tpm), Style::default().fg(color))
}

fn host_cell(host: &Host, w: &Widths) -> Span<'static> {
    match host {
        Host::Local => Span::styled(cell("local", w.host), Style::default().fg(Color::DarkGray)),
        Host::Remote { name, .. } => Span::styled(
            cell(name, w.host),
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        Host::Cloud => Span::styled(
            cell("cloud", w.host),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
    }
}

fn session_line(s: &Session, app: &App, w: &Widths) -> Line<'static> {
    let up = s
        .started_at
        .map(|t| format::ago(t, app.now_ms))
        .unwrap_or_else(|| "-".into());
    let model = s
        .model
        .as_deref()
        .unwrap_or("-")
        .trim_start_matches("claude-")
        .to_string();
    let toks = &s.tokens;
    let breakdown = format!(
        "{}/{}/{}/{}",
        format::tokens(toks.input),
        format::tokens(toks.output),
        format::tokens(toks.cache_write),
        format::tokens(toks.cache_read),
    );
    let expand = if s.agents.is_empty() {
        "  "
    } else if app.expanded.contains(&s.id) {
        "▾ "
    } else {
        "▸ "
    };
    let last = s
        .last_activity
        .map(|t| format::ago(t, app.now_ms))
        .unwrap_or_else(|| "-".into());
    // Prefer the human title (same names Claude's UI shows); slug in details.
    let display = s.title.as_deref().unwrap_or(&s.name);
    Line::from(vec![
        Span::styled(
            cell(&format!("{expand}{display}"), w.name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        host_cell(&s.host, w),
        state_span(s.state, w),
        Span::raw(cell(&up, w.up)),
        Span::styled(cell(&last, w.last), Style::default().fg(Color::Gray)),
        burn_span(s.tokens_per_min, BURN_RED, w),
        Span::styled(cell(&breakdown, w.breakdown), Style::default().fg(Color::Gray)),
        Span::raw(cell_r(&format!("{:.0}%", s.cpu_pct), w.cpu)),
        Span::raw(cell_r(&format!("{}M", s.rss_mb), w.rss)),
        Span::styled(format!("[{model}]"), Style::default().fg(Color::DarkGray)),
    ])
}

fn agent_line(
    a: Option<&ccwatch_core::model::Agent>,
    depth: usize,
    app: &App,
    w: &Widths,
) -> Line<'static> {
    let Some(a) = a else {
        return Line::from("· <agent>");
    };
    let indent = "  ".repeat(depth);
    let branch = if a.children.is_empty() {
        "└ "
    } else if app.expanded.contains(&a.id) {
        "▾ "
    } else {
        "▸ "
    };
    let (st, color) = match a.state {
        AgentState::Running => ("running", Color::Green),
        AgentState::Finished => ("done", Color::DarkGray),
    };
    let up = a
        .started_at
        .map(|t| format::ago(t, app.now_ms))
        .unwrap_or_else(|| "-".into());
    let last = a
        .last_activity
        .map(|t| format::ago(t, app.now_ms))
        .unwrap_or_else(|| "-".into());
    // The name area spans the NAME + HOST columns, so agent rows keep the
    // state/up columns aligned with session rows. A running agent's current
    // tool call rides along in the label.
    let name_area = w.name + w.host;
    let mut label = format!("{indent}{branch}{} [{}]", a.description, a.subagent_type);
    if matches!(a.state, AgentState::Running) {
        if let Some(act) = a.activity.first() {
            label.push_str(&format!(" · {} {}", act.tool, act.detail));
        }
    }
    let t = &a.tokens;
    let breakdown = if t.messages > 0 {
        format!(
            "{}/{}/{}/{}",
            format::tokens(t.input),
            format::tokens(t.output),
            format::tokens(t.cache_write),
            format::tokens(t.cache_read),
        )
    } else {
        "-".into()
    };
    let model = a
        .model
        .as_deref()
        .map(|m| format!("[{}]", m.trim_start_matches("claude-")))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(cell(&label, name_area), Style::default().fg(Color::Cyan)),
        Span::styled(cell(st, w.state), Style::default().fg(color)),
        Span::styled(cell(&up, w.up), Style::default().fg(Color::DarkGray)),
        Span::styled(cell(&last, w.last), Style::default().fg(Color::DarkGray)),
        burn_span(a.tokens_per_min, BURN_RED, w),
        Span::styled(cell(&breakdown, w.breakdown), Style::default().fg(Color::Gray)),
        Span::raw(cell_r("", w.cpu)),
        Span::raw(cell_r("", w.rss)),
        Span::styled(model, Style::default().fg(Color::DarkGray)),
    ])
}

fn draw_bottom_split(f: &mut Frame, area: Rect, app: &App) {
    let halves = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .split(area);
    let session = app.selected_session();

    // Tasks
    let (done, total) = session
        .map(|s| {
            (
                s.tasks.iter().filter(|t| t.status == "completed").count(),
                s.tasks.len(),
            )
        })
        .unwrap_or((0, 0));
    let tblock = Block::default()
        .borders(Borders::ALL)
        .title(format!(" TASKS ({done}/{total}) "));
    let titems: Vec<ListItem> = session
        .map(|s| s.tasks.iter().map(task_item).collect())
        .unwrap_or_default();
    f.render_widget(
        List::new(if titems.is_empty() {
            vec![ListItem::new(Span::styled("—", Style::default().fg(Color::DarkGray)))]
        } else {
            titems
        })
        .block(tblock),
        halves[0],
    );

    // Live work: in-flight tool calls (editing/reading/running — not OS
    // processes) followed by real child processes.
    let count = session
        .map(|s| s.activity.len() + s.processes.len())
        .unwrap_or(0);
    let pblock = Block::default()
        .borders(Borders::ALL)
        .title(format!(" ACTIVITY ({count}) "));
    let mut pitems: Vec<ListItem> = Vec::new();
    if let Some(s) = session {
        pitems.extend(s.activity.iter().map(|a| activity_item(a, app.now_ms)));
        pitems.extend(s.processes.iter().map(proc_item));
    }
    f.render_widget(
        List::new(if pitems.is_empty() {
            vec![ListItem::new(Span::styled(
                "no live activity",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            pitems
        })
        .block(pblock),
        halves[1],
    );

    // Watchers
    let wblock = Block::default().borders(Borders::ALL).title(" WATCHERS ");
    let witems: Vec<ListItem> = session
        .map(|s| s.watchers.iter().map(|w| watcher_item(w, app.now_ms)).collect())
        .unwrap_or_default();
    f.render_widget(
        List::new(if witems.is_empty() {
            vec![ListItem::new(Span::styled("—", Style::default().fg(Color::DarkGray)))]
        } else {
            witems
        })
        .block(wblock),
        halves[2],
    );
}

/// Icon for a tool kind: writing, reading/searching, running, fetching.
fn tool_icon(tool: &str) -> (&'static str, Color) {
    match tool {
        "Edit" | "Write" | "NotebookEdit" => ("✎", Color::Yellow),
        "Read" | "Grep" | "Glob" => ("⌕", Color::Cyan),
        "Bash" | "BashOutput" => ("⚙", Color::Green),
        "WebFetch" | "WebSearch" => ("⇣", Color::Magenta),
        _ => ("•", Color::Gray),
    }
}

fn activity_item(a: &ccwatch_core::model::Activity, now_ms: i64) -> ListItem<'static> {
    let (icon, color) = tool_icon(&a.tool);
    // Show the tail of paths/commands — that's the informative end.
    let detail: String = if a.detail.chars().count() > 34 {
        format!("…{}", a.detail.chars().rev().take(33).collect::<String>().chars().rev().collect::<String>())
    } else {
        a.detail.clone()
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{icon} "), Style::default().fg(color)),
        Span::styled(format!("{:<6}", a.tool), Style::default().fg(color)),
        Span::raw(format!("{detail} ")),
        Span::styled(
            format::ago(a.since_ms, now_ms),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
}

fn proc_item(p: &ccwatch_core::model::ProcInfo) -> ListItem<'static> {
    let cpu_color = if p.cpu_pct >= 50.0 {
        Color::Red
    } else if p.cpu_pct >= 10.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    ListItem::new(Line::from(vec![
        Span::styled("● ", Style::default().fg(cpu_color)),
        Span::styled(
            format!("{:<14}", truncate(&p.name, 14)),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("{:>4.0}% {:>5}M ", p.cpu_pct, p.rss_mb),
            Style::default().fg(cpu_color),
        ),
        Span::styled(
            format!("{} ", format::duration_ms(p.run_secs as i64 * 1000)),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
}

fn task_item(t: &ccwatch_core::model::Task) -> ListItem<'static> {
    let (glyph, color) = match t.status.as_str() {
        "completed" => ("✓", Color::Green),
        "in_progress" => ("●", Color::Yellow),
        _ => ("○", Color::Gray),
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::default().fg(color)),
        Span::raw(truncate(&t.subject, 32)),
    ];
    if t.blocked {
        spans.push(Span::styled(" ⛌blocked", Style::default().fg(Color::Red)));
    }
    ListItem::new(Line::from(spans))
}

fn watcher_item(w: &ccwatch_core::model::Watcher, now: i64) -> ListItem<'static> {
    let kind = match w.kind {
        WatcherKind::Hook => "hook",
        WatcherKind::Loop => "loop",
        WatcherKind::Routine => "routine",
        WatcherKind::Background => "bg",
    };
    let mut tail = String::new();
    if let Some(nw) = w.next_wake {
        tail = format!("  next {}", format::duration_ms(nw - now));
    } else if w.fired_count > 0 {
        tail = format!("  fired {}", w.fired_count);
    }
    ListItem::new(Line::from(vec![
        Span::styled(format!("{kind:<8}"), Style::default().fg(Color::Magenta)),
        Span::raw(format!("{:<8}", w.schedule.clone().unwrap_or_default())),
        Span::raw(truncate(&w.detail, 22)),
        Span::styled(tail, Style::default().fg(Color::DarkGray)),
    ]))
}

fn draw_details(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" DETAILS ");
    let Some(vr) = app.selected_row() else {
        f.render_widget(Paragraph::new("").block(block), area);
        return;
    };
    let lines: Vec<Line> = match &vr.row {
        RowRef::Session(si) => session_details(&app.snapshot.sessions[*si], app),
        RowRef::Agent(si, path) => match agent_at(&app.snapshot.sessions[*si].agents, path) {
            Some(a) => agent_details(a, &app.snapshot.sessions[*si], app),
            None => vec![Line::from("agent gone")],
        },
    };
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}

fn session_details(s: &Session, app: &App) -> Vec<Line<'static>> {
    let t = &s.tokens;
    vec![
        Line::from(vec![
            Span::styled("session ", Style::default().fg(Color::DarkGray)),
            Span::styled(s.name.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("   pid {}   {}   {}", s.pid.unwrap_or(0), s.kind, s.entrypoint)),
        ]),
        Line::from(format!("cwd {}", s.cwd)),
        Line::from(vec![
            Span::styled("tokens  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "in {} · out {} · cw {} · cr {}   msgs {}   {} tok/min",
                format::tokens(t.input),
                format::tokens(t.output),
                format::tokens(t.cache_write),
                format::tokens(t.cache_read),
                t.messages,
                format::rate(s.tokens_per_min),
            )),
        ]),
        Line::from(vec![
            Span::styled(
                "legend  in=input  out=output  cw=cache-write  cr=cache-read",
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(match s.last_activity {
                Some(la) => format!("   last activity {} ago", format::ago(la, app.now_ms)),
                None => String::new(),
            }),
        ]),
    ]
}

fn agent_details(a: &ccwatch_core::model::Agent, s: &Session, app: &App) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("agent ", Style::default().fg(Color::DarkGray)),
            Span::styled(a.description.clone(), Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(format!(
            "type {}   model {}   parent {}",
            a.subagent_type,
            a.model.as_deref().unwrap_or("-"),
            s.name
        )),
        Line::from(match a.started_at {
            Some(t) => format!(
                "started {}   ({} ago)   children {}",
                format::clock(t),
                format::ago(t, app.now_ms),
                a.children.len()
            ),
            None => format!("children {}", a.children.len()),
        }),
        Line::from(Span::styled(
            "note: subagents can't be stopped individually — kill the owning session",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let hint = match &app.mode {
        Mode::Fuzzy { .. } => " type to filter · ↑↓ move · enter jump · esc cancel ",
        Mode::Confirm(_) => " y confirm · n/esc cancel ",
        Mode::Details => " esc/d close ",
        Mode::Normal => {
            " / jump · ↑↓ · enter expand · d details · s sort · x hide-done · f hide-idle · k kill · p pause · r resume · q quit "
        }
    };
    let mut spans = vec![Span::styled(hint, Style::default().fg(Color::Black).bg(Color::Gray))];
    if let Some(st) = &app.status {
        spans.push(Span::styled(format!("  {st}"), Style::default().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Full-detail popup for the selected row (`d`).
fn draw_details_popup(f: &mut Frame, area: Rect, app: &App) {
    let popup = centered(72, 70, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" DETAILS (esc to close) ")
        .border_style(Style::default().fg(Color::Cyan));

    let mut lines: Vec<Line> = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);
    let mut kv = |k: &str, v: String| {
        lines.push(Line::from(vec![
            Span::styled(format!("{k:<14}"), dim),
            Span::raw(v),
        ]));
    };
    match app.selected_row().map(|vr| vr.row) {
        Some(RowRef::Session(si)) => {
            let s = &app.snapshot.sessions[si];
            let t = &s.tokens;
            kv("session", s.title.clone().unwrap_or_else(|| s.name.clone()));
            kv("slug", s.name.clone());
            kv("id", s.id.clone());
            kv("host", s.host.label());
            kv("pid", s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()));
            kv("kind", format!("{} · {} · v{}", s.kind, s.entrypoint, s.version));
            kv("model", s.model.clone().unwrap_or_else(|| "-".into()));
            kv("cwd", s.cwd.clone());
            if let Some(ts) = s.started_at {
                kv("started", format!("{} ({} ago)", format::clock(ts), format::ago(ts, app.now_ms)));
            }
            if let Some(ts) = s.last_activity {
                kv("last activity", format!("{} ago", format::ago(ts, app.now_ms)));
            }
            kv("burn", format!("{} tok/min", format::rate(s.tokens_per_min)));
            kv("tokens", format!(
                "in {} · out {} · cache-write {} · cache-read {}",
                format::tokens(t.input), format::tokens(t.output),
                format::tokens(t.cache_write), format::tokens(t.cache_read)));
            kv("messages", format!("{} · web search {} · web fetch {}", t.messages, t.web_search, t.web_fetch));
            kv("resources", format!("cpu {:.0}% · rss {} MB", s.cpu_pct, s.rss_mb));
            let running = s.agents.iter().filter(|a| matches!(a.state, AgentState::Running)).count();
            kv("agents", format!("{} running · {} total", running, s.agents.len()));
            let done = s.tasks.iter().filter(|t| t.status == "completed").count();
            kv("tasks", format!("{done}/{} done", s.tasks.len()));
            for w in &s.watchers {
                kv("watcher", format!("{:?} {} — {}", w.kind, w.name, w.detail));
            }
            for a in &s.activity {
                kv("activity", format!(
                    "{} {} — for {}",
                    a.tool, a.detail, format::ago(a.since_ms, app.now_ms)
                ));
            }
            for p in &s.processes {
                kv("process", format!(
                    "{} (pid {}) {:.0}% · {} MB · up {} — {}",
                    p.name, p.pid, p.cpu_pct, p.rss_mb,
                    format::duration_ms(p.run_secs as i64 * 1000),
                    p.cmd.chars().take(60).collect::<String>()
                ));
            }
        }
        Some(RowRef::Agent(si, path)) => {
            let s = &app.snapshot.sessions[si];
            if let Some(a) = agent_at(&s.agents, &path) {
                kv("agent", a.description.clone());
                kv("type", a.subagent_type.clone());
                kv("model", a.model.clone().unwrap_or_else(|| "-".into()));
                kv("state", match a.state { AgentState::Running => "running".into(), AgentState::Finished => "done".into() });
                if let Some(ts) = a.started_at {
                    kv("started", format!("{} ({} ago)", format::clock(ts), format::ago(ts, app.now_ms)));
                }
                kv("children", a.children.len().to_string());
                kv("parent", s.name.clone());
                kv("id", a.id.clone());
            }
        }
        None => lines.push(Line::from("nothing selected")),
    }
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_fuzzy(f: &mut Frame, area: Rect, query: &str, results: &[crate::app::JumpItem], cursor: usize) {
    let popup = centered(70, 60, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" jump (fuzzy) ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Cyan)),
            Span::raw(query.to_string()),
            Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ])),
        rows[0],
    );
    let h = rows[1].height as usize;
    let start = if cursor >= h { cursor - h + 1 } else { 0 };
    let items: Vec<ListItem> = results
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, it)| {
            let line = Line::from(vec![
                Span::styled(format!("{:<8}", it.kind), Style::default().fg(Color::DarkGray)),
                Span::raw(it.label.clone()),
            ]);
            if i == cursor {
                ListItem::new(line).style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                ListItem::new(line)
            }
        })
        .collect();
    f.render_widget(List::new(items), rows[1]);
}

fn draw_confirm(f: &mut Frame, area: Rect, prompt: &str) {
    let popup = centered(60, 20, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" confirm ")
        .border_style(Style::default().fg(Color::Red));
    f.render_widget(
        Paragraph::new(prompt.to_string())
            .block(block)
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Center),
        popup,
    );
}

fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(130, 40)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn app_with(snap: ccwatch_core::model::Snapshot) -> App {
        let mut a = App::new(10_000);
        a.connected = true;
        a.set_snapshot(snap);
        a
    }

    #[test]
    fn renders_full_layout() {
        let snap = snapshot(vec![session(
            "s1",
            "webapp",
            vec![agent("a1", "search dir", vec![])],
        )]);
        let app = app_with(snap);
        let s = render(&app);
        for needle in [
            "ccwatch", "active", "ALERTS", "runaway", "webapp", "running", "TASKS", "do the thing",
            "WATCHERS", "DETAILS", "cache-write", "ACTIVITY (2)", "cargo", "engine.rs",
        ] {
            assert!(s.contains(needle), "expected screen to contain {needle:?}\n{s}");
        }
    }

    /// Map a ratatui color to a hex string (catppuccin-ish palette), given the
    /// default to use for `Reset`.
    fn hex(c: Color, default: &str) -> String {
        match c {
            Color::Reset => default.to_string(),
            Color::Black => "#11111b".into(),
            Color::Red => "#f38ba8".into(),
            Color::Green => "#a6e3a1".into(),
            Color::Yellow => "#f9e2af".into(),
            Color::Blue => "#89b4fa".into(),
            Color::Magenta => "#f5c2e7".into(),
            Color::Cyan => "#94e2d5".into(),
            Color::Gray => "#bac2de".into(),
            Color::DarkGray => "#6c7086".into(),
            Color::White => "#cdd6f4".into(),
            _ => default.to_string(),
        }
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    /// Convert a rendered ratatui buffer into a standalone terminal-style SVG.
    fn buffer_to_svg(buf: &ratatui::buffer::Buffer) -> String {
        const CW: f64 = 8.4;
        const CH: f64 = 18.0;
        const PAD: f64 = 14.0;
        const TOP: f64 = 34.0;
        let cols = buf.area.width;
        let rows = buf.area.height;
        let w = cols as f64 * CW + PAD * 2.0;
        let h = rows as f64 * CH + PAD * 2.0 + TOP;
        let bg = "#1e1e2e";
        let default_fg = "#cdd6f4";

        let mut s = String::new();
        s.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w:.0} {h:.0}\" font-family=\"SFMono-Regular,Menlo,Consolas,monospace\" font-size=\"13\">\n"
        ));
        s.push_str(&format!(
            "<rect width=\"{w:.0}\" height=\"{h:.0}\" rx=\"10\" fill=\"{bg}\"/>\n"
        ));
        // Window chrome dots.
        for (i, c) in ["#f38ba8", "#f9e2af", "#a6e3a1"].iter().enumerate() {
            s.push_str(&format!(
                "<circle cx=\"{}\" cy=\"18\" r=\"6\" fill=\"{c}\"/>\n",
                PAD + 6.0 + i as f64 * 18.0
            ));
        }

        for y in 0..rows {
            let mut x = 0u16;
            while x < cols {
                let cell = &buf[(x, y)];
                let rev = cell.modifier.contains(Modifier::REVERSED);
                let (fg0, bg0) = (cell.fg, cell.bg);
                // Gather a run of same-style cells.
                let mut run = String::new();
                let startx = x;
                while x < cols {
                    let c = &buf[(x, y)];
                    if c.fg == fg0
                        && c.bg == bg0
                        && c.modifier.contains(Modifier::REVERSED) == rev
                    {
                        run.push_str(c.symbol());
                        x += 1;
                    } else {
                        break;
                    }
                }
                let (text_fg, rect_bg) = if rev {
                    (hex(bg0, bg), Some(hex(fg0, default_fg)))
                } else {
                    let rb = if matches!(bg0, Color::Reset) {
                        None
                    } else {
                        Some(hex(bg0, bg))
                    };
                    (hex(fg0, default_fg), rb)
                };
                let px = PAD + startx as f64 * CW;
                let py = TOP + PAD + y as f64 * CH;
                let rw = (x - startx) as f64 * CW;
                if let Some(rb) = rect_bg {
                    s.push_str(&format!(
                        "<rect x=\"{px:.1}\" y=\"{:.1}\" width=\"{rw:.1}\" height=\"{CH}\" fill=\"{rb}\"/>\n",
                        py - 13.0
                    ));
                }
                if !run.trim().is_empty() {
                    // textLength pins the run to the cell grid — otherwise the
                    // font's natural advance drifts and frame borders misalign.
                    s.push_str(&format!(
                        "<text x=\"{px:.1}\" y=\"{py:.1}\" xml:space=\"preserve\" textLength=\"{rw:.1}\" lengthAdjust=\"spacingAndGlyphs\" fill=\"{text_fg}\">{}</text>\n",
                        xml_escape(&run)
                    ));
                }
            }
        }
        s.push_str("</svg>\n");
        s
    }

    /// Render a rich, representative frame and write it to `docs/` as an SVG.
    /// Ignored by default; run with `cargo test -p ccwatch-tui emit_tui -- --ignored`.
    #[test]
    #[ignore]
    fn emit_tui_screenshot_svg() {
        use ccwatch_core::model::*;

        // A busy, representative scene: a hot local session with nested agents,
        // a quiet local one, and a remote host — plus alerts.
        let mut hot = session(
            "s1",
            "webapp",
            vec![{
                let mut e = agent("a1", "scan repository", vec![agent("a2", "sub-scan configs", vec![])]);
                e.subagent_type = "Explore".into();
                e
            }],
        );
        hot.title = Some("Fix flaky checkout tests".into());
        hot.tasks = vec![
            Task { subject: "bump dependencies".into(), status: "in_progress".into(), blocked: false, active_form: None },
            Task { subject: "add integration tests".into(), status: "pending".into(), blocked: true, active_form: None },
            Task { subject: "update changelog".into(), status: "completed".into(), blocked: false, active_form: None },
        ];

        let mut quiet = session("s2", "api-server", vec![]);
        quiet.title = Some("API rate limiter design".into());
        quiet.tokens_per_min = 1_000.0;

        let mut remote = session("s3", "remote-worker", vec![]);
        remote.title = Some("Overnight refactor bot".into());
        remote.host = Host::Remote { name: "demo-host".into(), ssh_target: "user@demo-host".into() };
        remote.remote_name = Some("demo-host".into());
        remote.tokens_per_min = 8_000.0;

        let mut snap = snapshot(vec![hot, quiet, remote]);
        // Governor segment in the top bar.
        let tank = ccwatch_core::model::Tank {
            used: 10_100_000,
            budget: Some(23_700_000),
            budget_source: ccwatch_core::model::BudgetSource::Learned,
            window_start: 0,
            resets_at: Some(12_000_000),
            rate_per_min: 62_000.0,
            cruise_per_min: Some(48_000.0),
            delta: Some(1.3),
            range_min: Some(219.0),
            wall_at: Some(9_000_000),
        };
        snap.governor = Some(ccwatch_core::model::GovernorStatus {
            window: tank,
            cruise: tank,
        });
        snap.alerts = vec![
            Alert { severity: Severity::Critical, kind: AlertKind::RunawayLoop, subject: "webapp".into(), session_id: "s1".into(), message: "62k tok/min · no user turn 7m · agent×2".into(), since_ms: 0 },
            Alert { severity: Severity::Warn, kind: AlertKind::AgentStorm, subject: "webapp".into(), session_id: "s1".into(), message: "2 agents spawned in 40s".into(), since_ms: 0 },
        ];
        snap.totals = Totals { active_sessions: 3, tokens_per_min: 71_000.0, total_tokens: 4_200_000, cache_hit_pct: 71.0 };

        let mut app = App::new(snap.generated_at);
        app.connected = true;
        app.set_snapshot(snap);
        app.expanded.insert("s1".into()); // show webapp's agents
        app.expanded.insert("a1".into()); // show the nested sub-scan
        app.move_selection(1); // land on the Explore agent → agent details

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(118, 32)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let svg = buffer_to_svg(terminal.backend().buffer());

        let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("screenshot-tui.svg"), svg).unwrap();
    }

    #[test]
    fn columns_align_across_local_and_remote_rows() {
        // A short-named local row, a long-named local row, and a remote row
        // with a long host — the state/tok columns must start at identical x.
        let mut long = session("s2", "a-very-long-session-name-here", vec![]);
        long.state = ccwatch_core::model::SessionState::Idle;
        let mut remote = session("s3", "remote-worker", vec![]);
        remote.title = Some("Overnight refactor bot".into());
        remote.host = ccwatch_core::model::Host::Remote {
            name: "very-long-hostname-xyz".into(),
            ssh_target: "u@h".into(),
        };
        remote.state = ccwatch_core::model::SessionState::Idle;
        let snap = snapshot(vec![session("s1", "tiny", vec![]), long, remote]);
        let app = app_with(snap);
        let screen = render(&app);
        // Only session rows (identified by their model tag) — the footer help
        // text also contains the word "idle".
        let lines: Vec<&str> = screen
            .lines()
            .filter(|l| l.contains("[opus-4-8]"))
            .collect();
        assert_eq!(lines.len(), 3, "expected 3 session rows\n{screen}");

        // Character columns, not byte offsets — rows contain multi-byte
        // glyphs (▸, …) before the state column.
        let col = |needle: &str| -> Vec<usize> {
            lines
                .iter()
                .filter_map(|l| l.find(needle).map(|b| l[..b].chars().count()))
                .collect()
        };
        // All three rows show a state word; every occurrence must share one x.
        let mut positions = col("running ");
        positions.extend(col("idle "));
        positions.sort_unstable();
        positions.dedup();
        assert_eq!(
            positions.len(),
            1,
            "state column misaligned, offsets {positions:?}\n{screen}"
        );
    }

    #[test]
    fn renders_host_tags_and_breakdown() {
        let mut remote = session("s2", "worker", vec![]);
        remote.host = ccwatch_core::model::Host::Remote {
            name: "demo-host".into(),
            ssh_target: "demo-host".into(),
        };
        let snap = snapshot(vec![session("s1", "webapp", vec![]), remote]);
        let app = app_with(snap);
        let s = render(&app);
        // Top-bar breakdown lists both hosts, and the remote row is tagged.
        assert!(s.contains("local 1"), "breakdown missing local:\n{s}");
        assert!(s.contains("demo-host"), "remote host tag/breakdown missing:\n{s}");
    }

    #[test]
    fn topbar_shows_governor_delta_and_wall() {
        use ccwatch_core::model::{BudgetSource, GovernorStatus, Tank};
        let mut snap = snapshot(vec![session("s1", "webapp", vec![])]);
        let tank = Tank {
            used: 500_000,
            budget: Some(1_000_000),
            budget_source: BudgetSource::Config,
            window_start: 0,
            resets_at: Some(10_000_000 + 3 * 3_600_000),
            rate_per_min: 10_000.0,
            cruise_per_min: Some(2_778.0),
            delta: Some(3.6),
            range_min: Some(50.0),
            wall_at: Some(10_000 + 50 * 60_000),
        };
        snap.governor = Some(GovernorStatus { window: tank, cruise: tank });
        let app = app_with(snap);
        let s = render(&app);
        assert!(s.contains("▲3.6×"), "delta missing from top bar:\n{s}");
        assert!(s.contains("wall"), "wall countdown missing:\n{s}");
    }

    #[test]
    fn renders_fuzzy_overlay() {
        let snap = snapshot(vec![session("s1", "webapp", vec![])]);
        let mut app = app_with(snap);
        app.open_fuzzy();
        app.fuzzy_input('b');
        let s = render(&app);
        assert!(s.contains("jump (fuzzy)"), "overlay missing:\n{s}");
    }

    #[test]
    fn renders_details_popup() {
        let snap = snapshot(vec![session("s1", "webapp", vec![agent("a1", "scan", vec![])])]);
        let mut app = app_with(snap);
        app.mode = crate::app::Mode::Details;
        let s = render(&app);
        assert!(s.contains("DETAILS (esc to close)"), "popup missing:\n{s}");
        assert!(s.contains("cache-write"), "token detail missing:\n{s}");
        assert!(s.contains("agents"), "agent summary missing:\n{s}");
    }

    #[test]
    fn renders_confirm_overlay() {
        let snap = snapshot(vec![session("s1", "webapp", vec![])]);
        let mut app = app_with(snap);
        app.stage_action(crate::app::ActionKind::Kill);
        let s = render(&app);
        assert!(s.contains("confirm"), "confirm overlay missing:\n{s}");
        assert!(s.contains("Kill session"), "prompt missing:\n{s}");
    }
}
