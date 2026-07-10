import SwiftUI

/// The menu-bar item's label: the live burn-rate load graph (colored by the
/// selected limit's throttle heat) plus the optional number.
struct TrayLabel: View {
    @ObservedObject var store: Store
    @AppStorage("limit_mode") private var limitRaw = LimitMode.mix.rawValue
    @AppStorage("menu_show") private var showRaw = MenuShow.throttle.rawValue

    var body: some View {
        let g = store.snap?.governor
        let mode = LimitMode(rawValue: limitRaw) ?? .mix
        let show = MenuShow(rawValue: showRaw) ?? .throttle
        let tank = g.map { Gov.selected($0, mode).tank }
        HStack(spacing: 3) {
            Image(nsImage: BurnGraph.image(store.burnHistory, heat: Gov.heat(tank?.delta)))
            if let tank, let txt = Gov.trayText(tank, show) {
                Text(txt)
            }
        }
    }
}

/// Popover panel shown from the menu bar. A live SwiftUI view (window style)
/// with NO NSMenu anywhere — settings are inline segmented controls and each
/// session expands inline — so live updates never collapse an open control.
struct MenuContent: View {
    @ObservedObject var store: Store
    let delegate: AppDelegate
    @AppStorage("limit_mode") private var limitRaw = LimitMode.mix.rawValue
    @AppStorage("menu_show") private var showRaw = MenuShow.throttle.rawValue
    @AppStorage("start_menu_bar_only") private var menuBarOnly = false
    @AppStorage("cruise_mode") private var cruiseMode = "off"
    @State private var showSettings = false

    var body: some View {
        VStack(spacing: 0) {
            if let snap = store.snap {
                header(snap)
                Divider()
                sessions(snap)
                Divider()
            } else {
                Text(store.connected ? "Waiting for data…" : "Connecting to ccwatchd…")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity).padding(.vertical, 20)
                Divider()
            }
            if showSettings {
                settingsPanel
                Divider()
            }
            footer
        }
        .frame(width: 370)
    }

    // MARK: - Governor header

    @ViewBuilder
    private func header(_ snap: Snapshot) -> some View {
        let mode = LimitMode(rawValue: limitRaw) ?? .mix
        VStack(alignment: .leading, spacing: 6) {
            if let g = snap.governor {
                let sel = Gov.selected(g, mode)
                HStack(alignment: .firstTextBaseline) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text("GOVERNOR · \(sel.label)").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
                        Text(Gov.throttleLabel(sel.tank.delta))
                            .font(.title2.weight(.bold).monospacedDigit())
                            .foregroundStyle(Gov.heatColor(Gov.heat(sel.tank.delta)))
                    }
                    Spacer()
                    VStack(alignment: .trailing, spacing: 1) {
                        if let f = g.window.usedFrac { Text(String(format: "5h  %.0f%%", f * 100)) }
                        if let wk = g.week, let f = wk.usedFrac { Text(String(format: "wk  %.0f%%", f * 100)) }
                    }.font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                }
            }
            Text("\(snap.totals.activeSessions) active · \(Fmt.rate(snap.totals.tokensPerMin)) · cache \(Int(snap.totals.cacheHitPct))%")
                .font(.caption).foregroundStyle(.secondary)
            if let g = snap.governor {
                Text("5h resets \(Fmt.clock(g.window.resetsAt)) · weekly resets \(Fmt.resetLabel(g.week?.resetsAt))")
                    .font(.caption2).foregroundStyle(.tertiary)
                paceLine(g, snap.generatedAt)
                // Cruise Control advisory: when the daemon is autonomously pacing
                // ("auto" mode), show the paced count with a one-key Release;
                // otherwise (advisory-only), show the first recommendation with Apply.
                if let plan = snap.pacing, plan.auto {
                    HStack(spacing: 6) {
                        Text("⚙︎ Cruise auto · \(plan.paced) paused"
                            + (plan.pausedRate > 0 ? " · \(Fmt.rate(plan.pausedRate)) held" : ""))
                            .font(.caption2).foregroundStyle(Palette.teal)
                        Spacer()
                        Button("Release") { store.setCruiseMode("off"); cruiseMode = "off" }
                            .buttonStyle(.borderless).font(.caption2).foregroundStyle(Palette.teal)
                    }
                } else if let plan = snap.pacing, !plan.actions.isEmpty,
                   let first = plan.actions.first(where: { $0.op == "pause" })?.reason {
                    HStack(spacing: 6) {
                        Text("⏸ Cruise · \(first)")
                            .font(.caption2).foregroundStyle(Palette.teal).lineLimit(2)
                        Spacer()
                        Button("Apply") { store.applyPacing() }
                            .buttonStyle(.borderless).font(.caption2).foregroundStyle(Palette.teal)
                    }
                }
                // Nudge to install the extension whenever the Governor lacks exact
                // (reported) usage — it's the only way to match Claude to the percent.
                if g.window.budgetSource != "reported"
                    && (g.week?.budgetSource ?? "unknown") != "reported" {
                    Link(destination: URL(string: "https://jagajaga.me/redline")!) {
                        Label("Install the browser extension for exact limits",
                              systemImage: "puzzlepiece.extension")
                    }
                    .font(.caption2).foregroundStyle(Palette.teal).buttonStyle(.plain)
                }
            }
            // Leak alerts only — the wall projection is already the pace line above.
            ForEach(snap.alerts.filter { $0.kind != "budget_wall" }.prefix(3)) { a in
                Text("⚠ \(a.subject): \(a.message)")
                    .font(.caption2)
                    .foregroundStyle(a.severity == "critical" ? Palette.red : Palette.orange)
                    .lineLimit(1)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
    }

    /// Projected exhaustion for the selected limit (5h / weekly / mix→soonest):
    /// its name, the projected run-out time, and whether it lands before reset.
    private func selectedExhaustion(_ g: GovernorStatus, _ now: Int64)
        -> (name: String, ex: Int64, before: Bool)? {
        func exhaust(_ t: Tank) -> Int64? {
            guard let r = t.rangeMin, t.ratePerMin > 0 else { return nil }
            return now + Int64(r * 60_000)
        }
        func proj(_ t: Tank?, _ name: String) -> (String, Int64, Bool)? {
            guard let t, let e = exhaust(t) else { return nil }
            return (name, e, t.resetsAt.map { e < $0 } ?? true)
        }
        switch LimitMode(rawValue: limitRaw) ?? .mix {
        case .window: return proj(g.window, "5h")
        case .week: return proj(g.week, "weekly")
        case .mix:
            // The binding limit — same one the hero throttle shows: whichever
            // you'll hit its wall first (highest delta). This is the limit that
            // runs out *before its own reset*, not merely the soonest clock time.
            let b = Gov.binding(g)
            return proj(b.tank, b.isWeek ? "weekly" : "5h")
        }
    }

    /// "<limit> tokens gone: TIME" for the selected limit — red if it hits
    /// before its reset, muted if it coasts.
    @ViewBuilder
    private func paceLine(_ g: GovernorStatus, _ now: Int64) -> some View {
        if let p = selectedExhaustion(g, now) {
            Text("\(p.before ? "⚠ " : "")\(p.name) tokens gone: \(Fmt.smartTime(p.ex, now: now))")
                .font(.caption2)
                .foregroundStyle(p.before ? Palette.red : Palette.dim)
        }
    }

    // MARK: - Active sessions (idle/done filtered out), each expandable

    @ViewBuilder
    private func sessions(_ snap: Snapshot) -> some View {
        // Only actively-working sessions here (no idle, no done).
        let live = snap.sessions
            .filter { isActive($0) }
            .sorted { $0.tokensPerMin > $1.tokensPerMin }
        if live.isEmpty {
            Text("Nothing active right now").font(.caption).foregroundStyle(.secondary)
                .frame(maxWidth: .infinity).padding(.vertical, 16)
        } else if live.count <= 5 {
            // Few sessions: size to content, no scrolling.
            sessionList(live)
        } else {
            // Many: scroll. (A ScrollView has no intrinsic height and would
            // collapse to 0 in a fit-to-content popover, so pin a height.)
            ScrollView { sessionList(live) }.frame(height: 440)
        }
    }

    private func sessionList(_ live: [Session]) -> some View {
        LazyVStack(spacing: 0) {
            ForEach(live) { s in
                SessionCard(session: s, store: store)
                Divider()
            }
        }
    }

    private func isActive(_ s: Session) -> Bool {
        let st = s.state.lowercased()
        return st == "running" || st == "waiting" || s.tokensPerMin > 0
    }

    // MARK: - Inline settings (segmented — no NSMenu to collapse)

    private var settingsPanel: some View {
        VStack(alignment: .leading, spacing: 8) {
            row("Limit") {
                Picker("", selection: $limitRaw) {
                    ForEach(LimitMode.allCases) { Text($0.seg).tag($0.rawValue) }
                }.pickerStyle(.segmented).labelsHidden()
            }
            row("Menu bar") {
                Picker("", selection: $showRaw) {
                    ForEach(MenuShow.allCases) { Text($0.seg).tag($0.rawValue) }
                }.pickerStyle(.segmented).labelsHidden()
            }
            row("Cruise") {
                Picker("", selection: $cruiseMode) {
                    Text("Off").tag("off")
                    Text("Advise").tag("advisory")
                    Text("Auto").tag("auto")
                }.pickerStyle(.segmented).labelsHidden()
                .onChange(of: cruiseMode) { _, m in store.setCruiseMode(m) }
            }
            Toggle("Start with menu bar only", isOn: $menuBarOnly)
                .font(.caption)
            Toggle("Start at login", isOn: Binding(
                get: { LoginItem.enabled },
                set: { LoginItem.set($0) }
            ))
            .font(.caption)
        }
        .padding(12)
    }

    private func row<C: View>(_ label: String, @ViewBuilder _ content: () -> C) -> some View {
        HStack {
            Text(label).font(.caption).foregroundStyle(.secondary).frame(width: 66, alignment: .leading)
            content()
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Button { withAnimation(.easeInOut(duration: 0.15)) { showSettings.toggle() } } label: {
                HStack(spacing: 4) {
                    Image(systemName: "gearshape")
                    Text("Settings")
                    Image(systemName: "chevron.down").font(.caption2)
                        .rotationEffect(.degrees(showSettings ? 180 : 0))
                }
            }
            .buttonStyle(.borderless)
            Spacer()
            Button("TUI") { delegate.openTUI() }
            Button("Dashboard") { delegate.showDashboard() }
            Button("Quit") { delegate.quit() }
        }
        .font(.callout)
        .padding(.horizontal, 12).padding(.vertical, 8)
    }
}

/// Rich per-session row for the menu-bar popover, expandable for deeper detail.
struct SessionCard: View {
    let session: Session
    let store: Store
    @State private var expanded = false

    var body: some View {
        let s = session
        let total = countAgents(s.agents, state: nil)
        let running = countAgents(s.agents, state: "running")
        VStack(alignment: .leading, spacing: 3) {
            // Summary line — click anywhere to expand.
            Button { expanded.toggle() } label: {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Image(systemName: expanded ? "chevron.down" : "chevron.right")
                            .font(.system(size: 9)).foregroundStyle(.secondary)
                        Circle().fill(s.pausedByCruise ? Palette.orange : Fmt.stateColor(s.state))
                            .frame(width: 8, height: 8)
                        Text(s.title ?? s.name).fontWeight(.medium).lineLimit(1)
                        if s.pausedByCruise {
                            Text("⏸ PAUSED").font(.caption2.weight(.bold))
                                .foregroundStyle(Palette.orange)
                                .padding(.horizontal, 4).padding(.vertical, 1)
                                .background(Palette.orange.opacity(0.18))
                                .clipShape(Capsule())
                        }
                        Spacer()
                        Text(Fmt.rate(s.tokensPerMin))
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(s.pausedByCruise ? Palette.orange : .secondary)
                    }
                    HStack(spacing: 8) {
                        if let m = s.model { Text(tier(m)).foregroundStyle(Fmt.tierColor(tier(m))) }
                        Text(Fmt.tokens(s.tokens.total) + " tok")
                        Text(String(format: "%.0f%% cpu", s.cpuPct))
                        if s.rssMb > 0 { Text("\(s.rssMb) MB") }
                        Text(hostLabel(s))
                    }.font(.caption2).foregroundStyle(.secondary)
                }
            }
            .buttonStyle(.plain)

            if let act = s.activity.first {
                detailLine("wrench.and.screwdriver", "\(act.tool) \(act.detail)", Palette.teal)
            }
            if total > 0 {
                Text("\(total) agent\(total == 1 ? "" : "s") · \(running) running")
                    .font(.caption2).foregroundStyle(.secondary)
            }

            if expanded { detail(s) }

            if let pid = s.pid {
                HStack(spacing: 12) {
                    Button("Kill") { store.killSession(pid: pid) }
                    Button("Pause") { store.pauseSession(pid: pid) }
                    Button("Resume") { store.resumeSession(pid: pid) }
                    Spacer()
                    // Cruise priority pin. Default (no override) = Normal: Cruise
                    // decides. High = never pause; Low = shed first.
                    Menu(cruiseLabel(s)) {
                        Button("High · never pause") { store.setSessionPriority(s.id, "high") }
                        Button("Normal · Cruise decides") { store.setSessionPriority(s.id, "normal") }
                        Button("Low · shed first") { store.setSessionPriority(s.id, "low") }
                    }
                    .menuStyle(.borderlessButton).fixedSize()
                }
                .buttonStyle(.borderless).font(.caption2).foregroundStyle(Palette.teal)
                .padding(.top, 1)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 12).padding(.vertical, 7)
    }

    /// The Cruise-priority menu label reflects the current per-session override.
    private func cruiseLabel(_ s: Session) -> String {
        switch s.priorityOverride {
        case "high": return "⇧ Cruise: High"
        case "low": return "⇩ Cruise: Low"
        default: return "Cruise: auto"
        }
    }

    // MARK: - Expanded detail

    @ViewBuilder
    private func detail(_ s: Session) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text("in \(Fmt.tokens(s.tokens.input)) · out \(Fmt.tokens(s.tokens.output)) · cache \(Fmt.tokens(s.tokens.cacheRead)) · \(s.tokens.messages) msgs")
                .font(.caption2).foregroundStyle(.secondary)
            HStack(spacing: 8) {
                if let st = s.startedAt { Text("started \(Fmt.clock(st))") }
                if let la = s.lastActivity { Text("last \(Fmt.clock(la))") }
                if !s.cwd.isEmpty { Text((s.cwd as NSString).lastPathComponent) }
            }.font(.caption2).foregroundStyle(.tertiary)

            ForEach(flatten(s.agents).filter { $0.2.state.lowercased() == "running" }, id: \.0) { key, depth, a in
                HStack(spacing: 4) {
                    Image(systemName: "bolt.fill").font(.system(size: 8)).foregroundStyle(Palette.green)
                    Text(a.subagentType.isEmpty ? "agent" : a.subagentType).fontWeight(.medium)
                    if let act = a.activity.first { Text("· \(act.tool) \(act.detail)").foregroundStyle(.secondary).lineLimit(1) }
                    Spacer()
                    if a.tokensPerMin > 0 { Text(Fmt.rate(a.tokensPerMin)).foregroundStyle(.tertiary) }
                    Text(Fmt.tokens(a.tokens.total)).foregroundStyle(.secondary)
                }
                .font(.caption2)
                .padding(.leading, CGFloat(depth) * 10)
                .id(key)
            }
            ForEach(Array(s.tasks.enumerated()), id: \.offset) { _, t in
                detailLine("checklist", "\(t.subject) [\(t.status)]", .secondary)
            }
            ForEach(s.processes) { p in
                detailLine("gearshape", "\(p.name) \(p.cmd) · \(Int(p.cpuPct))%", .secondary)
            }
        }
        .padding(.leading, 14).padding(.top, 2)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func detailLine(_ icon: String, _ text: String, _ color: Color) -> some View {
        HStack(spacing: 4) {
            Image(systemName: icon).font(.system(size: 9))
            Text(text).lineLimit(1)
        }.font(.caption2).foregroundStyle(color)
    }

    // MARK: - Helpers

    /// Depth-first flatten of the agent tree → (stableKey, depth, agent).
    private func flatten(_ agents: [Agent], depth: Int = 0, prefix: String = "") -> [(String, Int, Agent)] {
        var out: [(String, Int, Agent)] = []
        for (i, a) in agents.enumerated() {
            let key = "\(prefix)\(i)-\(a.id)"
            out.append((key, depth, a))
            out.append(contentsOf: flatten(a.children, depth: depth + 1, prefix: key + "/"))
        }
        return out
    }
    private func countAgents(_ agents: [Agent], state: String?) -> Int {
        agents.reduce(0) { acc, a in
            let hit = state == nil || a.state.lowercased() == state
            return acc + (hit ? 1 : 0) + countAgents(a.children, state: state)
        }
    }
    private func tier(_ m: String) -> String {
        let l = m.lowercased()
        if l.contains("opus") { return "opus" }
        if l.contains("fable") { return "fable" }
        if l.contains("sonnet") { return "sonnet" }
        if l.contains("haiku") { return "haiku" }
        return "default"
    }
    private func hostLabel(_ s: Session) -> String {
        s.host.kind == "local" ? "local" : (s.host.name ?? s.remoteName ?? s.host.kind)
    }
}
