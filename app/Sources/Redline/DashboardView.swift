import SwiftUI

struct DashboardView: View {
    @ObservedObject var store: Store
    @AppStorage("start_menu_bar_only") private var menuBarOnly = false
    @AppStorage("limit_mode") private var limitRaw = LimitMode.mix.rawValue
    @AppStorage("hide_done") private var hideDone = true
    @AppStorage("hide_inactive") private var hideInactive = false

    var body: some View {
        ZStack {
            VisualEffectView().ignoresSafeArea()
            content
        }
        .frame(minWidth: 720, minHeight: 560)
    }

    @ViewBuilder
    private var content: some View {
        if let snap = store.snap {
            VStack(spacing: 0) {
                header(snap)
                AlertsBanner(alerts: snap.alerts)
                Divider().opacity(0.4)
                SessionTree(snap: snap, store: store,
                            hideDone: hideDone, hideInactive: hideInactive)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                footer
            }
        } else {
            VStack(spacing: 10) {
                ProgressView()
                Text(store.connected ? "Waiting for the first snapshot…" : "Connecting to ccwatchd…")
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    // MARK: - Header (hero governor + limits + mix + totals)

    private func header(_ snap: Snapshot) -> some View {
        HStack(alignment: .top, spacing: 20) {
            governorHero(snap)
            VStack(alignment: .leading, spacing: 14) {
                LimitsCard(governor: snap.governor)
                // Recommend the extension while the Governor runs on estimates.
                if let g = snap.governor,
                   g.window.budgetSource != "reported",
                   (g.week?.budgetSource ?? "unknown") != "reported" {
                    Link(destination: URL(string: "https://jagajaga.me/redline")!) {
                        Label("Install the browser extension for exact limits",
                              systemImage: "puzzlepiece.extension")
                    }
                    .font(.caption).foregroundStyle(Palette.teal).buttonStyle(.plain)
                }
                MixBar(mix: snap.modelMix)
                TotalsStrip(totals: snap.totals, alerts: snap.alerts)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(20)
    }

    @ViewBuilder
    private func governorHero(_ snap: Snapshot) -> some View {
        if let g = snap.governor {
            let mode = LimitMode(rawValue: limitRaw) ?? .mix
            let sel = Gov.selected(g, mode)
            let tank = sel.tank
            let heat = Gov.heat(tank.delta)
            let sub: String = {
                if let w = tank.wallAt {
                    return "hits ~\(mode == .week ? Fmt.resetLabel(w) : Fmt.clock(w))"
                }
                if let r = tank.resetsAt {
                    return "resets \(mode == .week ? Fmt.resetLabel(r) : Fmt.clock(r))"
                }
                return sel.label.lowercased()
            }()
            VStack(spacing: 8) {
                GovernorRing(frac: tank.usedFrac ?? min(1, heat),
                             color: Gov.heatColor(heat),
                             big: Gov.throttleLabel(tank.delta),
                             sub: sub)
                    .frame(width: 168, height: 168)
                Text(sel.label)
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
            }
        } else {
            VStack(spacing: 8) {
                GovernorRing(frac: 0, color: Palette.dim, big: "—", sub: "no governor")
                    .frame(width: 168, height: 168)
            }
        }
    }

    // MARK: - Footer

    private var footer: some View {
        HStack(spacing: 16) {
            Circle().fill(store.connected ? Palette.green : Palette.red)
                .frame(width: 8, height: 8)
            Text(store.connected ? "Live" : "Disconnected")
                .font(.caption).foregroundStyle(.secondary)
            Spacer()
            Toggle("Hide inactive", isOn: $hideInactive)
                .toggleStyle(.checkbox).font(.caption)
            Toggle("Hide done", isOn: $hideDone)
                .toggleStyle(.checkbox).font(.caption)
            Divider().frame(height: 14)
            Toggle("Start with menu bar only", isOn: $menuBarOnly)
                .toggleStyle(.checkbox).font(.caption)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 10)
    }
}

// MARK: - Governor ring

struct GovernorRing: View {
    let frac: Double
    let color: Color
    let big: String
    let sub: String

    var body: some View {
        ZStack {
            Canvas { ctx, size in
                let lw: CGFloat = 16
                let inset = lw / 2 + 2
                let center = CGPoint(x: size.width / 2, y: size.height / 2)
                let radius = min(size.width, size.height) / 2 - inset
                var track = Path()
                track.addArc(center: center, radius: radius,
                             startAngle: .degrees(0), endAngle: .degrees(360), clockwise: false)
                ctx.stroke(track, with: .color(Palette.dim.opacity(0.18)),
                           style: StrokeStyle(lineWidth: lw))
                let f = max(0, min(1, frac))
                if f > 0 {
                    var arc = Path()
                    arc.addArc(center: center, radius: radius,
                               startAngle: .degrees(-90),
                               endAngle: .degrees(-90 + 360 * f), clockwise: false)
                    ctx.stroke(arc, with: .color(color),
                               style: StrokeStyle(lineWidth: lw, lineCap: .round))
                }
            }
            VStack(spacing: 2) {
                Text(big)
                    .font(.system(size: 34, weight: .bold, design: .rounded))
                    .foregroundStyle(color)
                Text(sub)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

// MARK: - Limits card (both tanks side by side)

struct LimitsCard: View {
    let governor: GovernorStatus?

    var body: some View {
        HStack(spacing: 14) {
            tank("5H WINDOW", governor?.window, isWeek: false)
            tank("WEEKLY", governor?.week, isWeek: true)
        }
        .cardStyle()
    }

    @ViewBuilder
    private func tank(_ title: String, _ t: Tank?, isWeek: Bool) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
            let frac = t?.usedFrac ?? 0
            let heat = Gov.heat(t?.delta)
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    Capsule().fill(Palette.dim.opacity(0.18))
                    Capsule().fill(Gov.heatColor(heat))
                        .frame(width: max(4, geo.size.width * frac))
                }
            }
            .frame(height: 10)
            HStack {
                Text(t.flatMap { $0.usedFrac } != nil ? String(format: "%.0f%%", frac * 100) : "learning")
                    .font(.caption.monospacedDigit())
                Spacer()
                if let r = t?.resetsAt {
                    Text("↺ \(isWeek ? Fmt.resetLabel(r) : Fmt.clock(r))")
                        .font(.caption2).foregroundStyle(.secondary)
                }
            }
            if let w = t?.wallAt {
                Text("⚠ hits ~\(isWeek ? Fmt.resetLabel(w) : Fmt.clock(w)) at this pace")
                    .font(.caption2).foregroundStyle(Palette.red)
            }
        }
        .frame(maxWidth: .infinity)
    }
}

// MARK: - Model mix bar

struct MixBar: View {
    let mix: [MixEntry]

    var body: some View {
        let sorted = mix.sorted { $0.tokens > $1.tokens }
        let total = max(1, sorted.reduce(0) { $0 + $1.tokens })
        VStack(alignment: .leading, spacing: 6) {
            Text("MODEL MIX").font(.caption2.weight(.semibold)).foregroundStyle(.secondary)
            GeometryReader { geo in
                HStack(spacing: 1) {
                    ForEach(sorted) { e in
                        Fmt.tierColor(e.tier)
                            .frame(width: geo.size.width * Double(e.tokens) / Double(total))
                    }
                }
                .clipShape(Capsule())
            }
            .frame(height: 10)
            HStack(spacing: 12) {
                ForEach(sorted) { e in
                    HStack(spacing: 4) {
                        Circle().fill(Fmt.tierColor(e.tier)).frame(width: 7, height: 7)
                        Text(e.tier).font(.caption2).foregroundStyle(.secondary)
                    }
                }
            }
        }
        .cardStyle()
    }
}

// MARK: - Alerts

struct AlertsBanner: View {
    let alerts: [Alert]

    var body: some View {
        // Leak alerts only; the wall projection lives in the Governor gauge.
        let leaks = alerts.filter { $0.kind != "budget_wall" }
        if leaks.isEmpty {
            EmptyView()
        } else {
            banner(leaks)
        }
    }

    private func banner(_ leaks: [Alert]) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(leaks) { a in
                HStack(spacing: 8) {
                    Image(systemName: a.severity == "critical" ? "exclamationmark.octagon.fill" : "exclamationmark.triangle.fill")
                        .foregroundStyle(a.severity == "critical" ? Palette.red : Palette.orange)
                        .font(.caption)
                    Text(a.subject).fontWeight(.medium)
                    Text(a.message).foregroundStyle(.secondary).lineLimit(1)
                    Spacer()
                    Text(Fmt.ago(a.sinceMs)).foregroundStyle(.tertiary)
                }
                .font(.caption)
            }
        }
        .padding(.horizontal, 20).padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Palette.orange.opacity(0.08))
    }
}

// MARK: - Totals

struct TotalsStrip: View {
    let totals: Totals
    let alerts: [Alert]

    var body: some View {
        HStack(spacing: 18) {
            stat("\(totals.activeSessions)", "active")
            stat(Fmt.rate(totals.tokensPerMin), "burn")
            stat(String(format: "%.0f%%", totals.cacheHitPct), "cache")
            let leaks = alerts.filter { $0.kind != "budget_wall" }
            if !leaks.isEmpty {
                Spacer()
                HStack(spacing: 5) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Palette.orange)
                    Text("\(leaks.count)").font(.callout.weight(.semibold))
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func stat(_ value: String, _ label: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(value).font(.callout.weight(.semibold).monospacedDigit())
            Text(label).font(.caption2).foregroundStyle(.secondary)
        }
    }
}

extension View {
    func cardStyle() -> some View {
        self.padding(12)
            .background(Color.primary.opacity(0.05))
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(Color.primary.opacity(0.08))
            )
    }
}
