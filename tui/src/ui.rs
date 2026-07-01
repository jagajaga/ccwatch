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
        Mode::Normal => {}
    }
}

fn draw_topbar(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.snapshot.totals;
    let conn = if app.connected { "●" } else { "○ disconnected" };
    let text = format!(
        " ccwatch  {conn}  {} · {} active · {} tok/min · Σ {} · cache {:.0}%",
        host_breakdown(app),
        t.active_sessions,
        format::rate(t.tokens_per_min),
        format::tokens(t.total_tokens),
        t.cache_hit_pct,
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

/// Fixed column widths for the session/agent grid — every row pads to these,
/// so columns stay aligned no matter how long a name, host, or token figure is.
const W_NAME: usize = 24; // includes the 2-char expand marker
const W_HOST: usize = 13;
const W_STATE: usize = 8;
const W_UP: usize = 8;
const W_TPM: usize = 8;
const W_BREAKDOWN: usize = 27;
const W_CPU: usize = 5;
const W_RSS: usize = 6;

/// Truncate-and-pad to an exact width (left-aligned).
fn cell(s: &str, w: usize) -> String {
    format!("{:<w$}", truncate(s, w.saturating_sub(1)), w = w)
}

/// Truncate-and-pad to an exact width (right-aligned, one trailing space).
fn cell_r(s: &str, w: usize) -> String {
    format!("{:>w$} ", truncate(s, w.saturating_sub(1)), w = w.saturating_sub(1))
}

fn draw_tree(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.visible_rows();
    let header = format!(
        "{}{}{}{}{}{}{}{}model",
        cell("  name/desc", W_NAME),
        cell("host", W_HOST),
        cell("state", W_STATE),
        cell("up", W_UP),
        cell_r("tok/min", W_TPM),
        cell("in/out/cw/cr", W_BREAKDOWN),
        cell_r("cpu", W_CPU),
        cell_r("rss", W_RSS),
    );
    let block = Block::default().borders(Borders::ALL).title(Line::from(vec![
        Span::raw(" SESSIONS / AGENTS "),
        Span::styled(header, Style::default().fg(Color::DarkGray)),
    ]));
    let inner_h = area.height.saturating_sub(2) as usize;
    let start = if app.selected >= inner_h {
        app.selected - inner_h + 1
    } else {
        0
    };

    let mut items = Vec::new();
    for (i, vr) in rows.iter().enumerate().skip(start).take(inner_h) {
        let selected = i == app.selected;
        let line = match &vr.row {
            RowRef::Session(si) => session_line(&app.snapshot.sessions[*si], app),
            RowRef::Agent(si, path) => {
                let a = agent_at(&app.snapshot.sessions[*si].agents, path);
                agent_line(a, vr.depth, app)
            }
        };
        let item = if selected {
            ListItem::new(line).style(Style::default().add_modifier(Modifier::REVERSED))
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
    f.render_widget(List::new(items).block(block), area);
}

fn state_span(state: SessionState) -> Span<'static> {
    let (txt, color) = match state {
        SessionState::Running => ("running", Color::Green),
        SessionState::Idle => ("idle", Color::DarkGray),
        SessionState::Ended => ("ended", Color::DarkGray),
    };
    Span::styled(cell(txt, W_STATE), Style::default().fg(color))
}

fn burn_span(tpm: f64, threshold: f64) -> Span<'static> {
    let color = if tpm >= threshold {
        Color::Red
    } else if tpm >= threshold / 2.0 {
        Color::Yellow
    } else {
        Color::Gray
    };
    Span::styled(cell_r(&format::rate(tpm), W_TPM), Style::default().fg(color))
}

fn host_cell(host: &Host) -> Span<'static> {
    match host {
        Host::Local => Span::styled(cell("local", W_HOST), Style::default().fg(Color::DarkGray)),
        Host::Remote { name, .. } => Span::styled(
            cell(name, W_HOST),
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        Host::Cloud => Span::styled(
            cell("cloud", W_HOST),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
    }
}

fn session_line(s: &Session, app: &App) -> Line<'static> {
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
    Line::from(vec![
        Span::styled(
            cell(&format!("{expand}{}", s.name), W_NAME),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        host_cell(&s.host),
        state_span(s.state),
        Span::raw(cell(&up, W_UP)),
        burn_span(s.tokens_per_min, BURN_RED),
        Span::styled(cell(&breakdown, W_BREAKDOWN), Style::default().fg(Color::DarkGray)),
        Span::raw(cell_r(&format!("{:.0}%", s.cpu_pct), W_CPU)),
        Span::raw(cell_r(&format!("{}M", s.rss_mb), W_RSS)),
        Span::styled(format!("[{model}]"), Style::default().fg(Color::DarkGray)),
    ])
}

fn agent_line(a: Option<&ccwatch_core::model::Agent>, depth: usize, app: &App) -> Line<'static> {
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
    // The name area spans the NAME + HOST columns, so agent rows keep the
    // state/up columns aligned with session rows.
    let name_area = W_NAME + W_HOST;
    let label = format!("{indent}{branch}{} [{}]", a.description, a.subagent_type);
    Line::from(vec![
        Span::styled(cell(&label, name_area), Style::default().fg(Color::Cyan)),
        Span::styled(cell(st, W_STATE), Style::default().fg(color)),
        Span::styled(cell(&up, W_UP), Style::default().fg(Color::DarkGray)),
    ])
}

fn draw_bottom_split(f: &mut Frame, area: Rect, app: &App) {
    let halves = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
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
        halves[1],
    );
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
        Mode::Normal => {
            " / jump · ↑↓ move · enter expand · k kill · p pause · r resume · f filter-idle · q quit "
        }
    };
    let mut spans = vec![Span::styled(hint, Style::default().fg(Color::Black).bg(Color::Gray))];
    if let Some(st) = &app.status {
        spans.push(Span::styled(format!("  {st}"), Style::default().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
            "WATCHERS", "DETAILS", "cache-write",
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
                    s.push_str(&format!(
                        "<text x=\"{px:.1}\" y=\"{py:.1}\" xml:space=\"preserve\" fill=\"{text_fg}\">{}</text>\n",
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
        let mut webapp = session(
            "s1",
            "webapp",
            vec![{
                let mut e = agent("a1", "search ~/.claude", vec![agent("a2", "sub-scan configs", vec![])]);
                e.subagent_type = "Explore".into();
                e
            }],
        );
        webapp.tasks = vec![
            Task { subject: "bump dependencies".into(), status: "in_progress".into(), blocked: false, active_form: None },
            Task { subject: "audit deploy".into(), status: "pending".into(), blocked: true, active_form: None },
            Task { subject: "update changelog".into(), status: "completed".into(), blocked: false, active_form: None },
        ];

        let mut quiet = session("s2", "ccwatch", vec![]);
        quiet.tokens_per_min = 1_000.0;

        let mut remote = session("s3", "remote-worker", vec![]);
        remote.host = Host::Remote { name: "demo-host".into(), ssh_target: "user@demo-host".into() };
        remote.remote_name = Some("demo-host".into());
        remote.tokens_per_min = 8_000.0;

        let mut snap = snapshot(vec![webapp, quiet, remote]);
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
    fn renders_fuzzy_overlay() {
        let snap = snapshot(vec![session("s1", "webapp", vec![])]);
        let mut app = app_with(snap);
        app.open_fuzzy();
        app.fuzzy_input('b');
        let s = render(&app);
        assert!(s.contains("jump (fuzzy)"), "overlay missing:\n{s}");
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
