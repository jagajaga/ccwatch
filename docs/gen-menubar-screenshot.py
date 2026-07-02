#!/usr/bin/env python3
# Regenerates docs/screenshot-menubar.svg to match menubar/src/graph.rs +
# summary.rs: rounded translucent well, teal→amber→red Apple ramp with vertical
# fade, and the submenu with an area sparkline + actions.

BURN = 40_000.0

def gradient(v):
    lerp = lambda a, b, t: round(a + (b - a) * t)
    v = max(0.0, min(1.0, v))
    if v < 0.6:
        t = v / 0.6
        return (lerp(48, 255, t), lerp(176, 159, t), lerp(199, 10, t))
    t = (v - 0.6) / 0.4
    return (255, lerp(159, 69, t), lerp(10, 58, t))

hist = [8,11,13,16,22,26,30,34,32,38,42,46]
hist = [h * 1000 for h in hist]
peak = max(max(hist), BURN)

W, STRIP_H, TOP = 640, 30, 8
GH = 14
BAR, GAP = 2, 1
svg = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} 330" '
       f'font-family="-apple-system,BlinkMacSystemFont,Helvetica,Arial,sans-serif">']

svg.append(f'<rect x="0" y="{TOP}" width="{W}" height="{STRIP_H}" rx="7" fill="#1d1d1f"/>')
n = len(hist)
well_w = n * (BAR + GAP) + GAP + 4
well_h = 18
cx = W - well_w - 165
wy = TOP + (STRIP_H - well_h) / 2
# the well
svg.append(f'<rect x="{cx}" y="{wy}" width="{well_w}" height="{well_h}" rx="3" fill="#7f7f84" opacity="0.30"/>')
base_y = wy + well_h - 2
for i, s in enumerate(hist):
    vh = s / peak
    vc = s / BURN
    r, g, b = gradient(vc)
    hgt = max(1, round(vh * (well_h - 4)))
    x = cx + 2 + GAP + i * (BAR + GAP)
    # vertical fade: draw as gradient-less two-tone (cap + body opacity)
    svg.append(f'<rect x="{x}" y="{base_y-hgt}" width="{BAR}" height="{min(1.5,hgt)}" fill="rgb({r},{g},{b})"/>')
    if hgt > 1.5:
        svg.append(f'<rect x="{x}" y="{base_y-hgt+1.5}" width="{BAR}" height="{hgt-1.5}" fill="rgb({r},{g},{b})" opacity="0.72"/>')
svg.append(f'<text x="{cx+well_w+8}" y="{TOP+20}" fill="#f2f2f7" font-size="13" font-variant-numeric="tabular-nums">46k</text>')
svg.append(f'<text x="{W-14}" y="{TOP+20}" fill="#c7c7cc" font-size="13" text-anchor="end">Mon 21:14</text>')

# dropdown
DW = 300
dx = W - DW - 280
dy = TOP + STRIP_H + 6
rows = [
    ("hdr",  "local 2 · demo-host 1 · 46k tok/min · Σ 4.2M · cache 91%"),
    ("hdr",  "throttle ▲1.3× · range 3h39m · tank ~57% · reset 06:10"),
    ("alert","⚠ runaway loop — webapp: 62k tok/min · no user turn 7m"),
    ("sess", "Fix flaky checkout tests  —  24k tok/min", True),
    ("sess", "API rate limiter design  —  16k tok/min", False),
    ("sess", "Overnight refactor bot  —  8k · demo-host", False),
    ("item", "Settings                                              ›"),
    ("item", "Open TUI dashboard"),
    ("quit", "Quit ccwatch"),
]
row_h = 30
dh = row_h * len(rows) + 8
svg.append(f'<rect x="{dx}" y="{dy}" width="{DW}" height="{dh}" rx="10" fill="#ffffff" stroke="#d2d2d7"/>')
y = dy + 4
sel_y = None
for row in rows:
    kind, label = row[0], row[1]
    cy = y + row_h / 2 + 4.5
    if kind == "hdr":
        svg.append(f'<text x="{dx+14}" y="{cy}" fill="#6e6e73" font-size="12">{label}</text>')
        if "throttle" in label:
            svg.append(f'<line x1="{dx+10}" y1="{y+row_h}" x2="{dx+DW-10}" y2="{y+row_h}" stroke="#e5e5ea"/>')
    elif kind == "alert":
        svg.append(f'<text x="{dx+14}" y="{cy}" fill="#d70015" font-size="12.5">{label[:52]}</text>')
    elif kind == "sess":
        selected = row[2]
        if selected:
            sel_y = y
            svg.append(f'<rect x="{dx+5}" y="{y+2}" width="{DW-10}" height="{row_h-4}" rx="6" fill="#0a60ff"/>')
            fill, sub = "#ffffff", "#d8e5ff"
        else:
            fill, sub = "#1d1d1f", "#8e8e93"
        name, rest = label.split("  —  ", 1)
        svg.append(f'<text x="{dx+14}" y="{cy}" font-size="13.5" fill="{fill}">{name}'
                   f'<tspan fill="{sub}" font-size="12">   {rest}</tspan></text>')
        svg.append(f'<text x="{dx+DW-12}" y="{cy}" fill="{sub}" font-size="12" text-anchor="end">›</text>')
    elif kind == "item":
        svg.append(f'<line x1="{dx+10}" y1="{y}" x2="{dx+DW-10}" y2="{y}" stroke="#e5e5ea"/>')
        svg.append(f'<text x="{dx+14}" y="{cy}" fill="#1d1d1f" font-size="13.5">{label}</text>')
    else:
        svg.append(f'<text x="{dx+14}" y="{cy}" fill="#1d1d1f" font-size="13.5">{label}</text>')
    y += row_h

# submenu with area sparkline
SW = 268
sx = dx + DW + 4
sy = sel_y
sh = 26 * 8 + 14
svg.append(f'<rect x="{sx}" y="{sy}" width="{SW}" height="{sh}" rx="10" fill="#ffffff" stroke="#d2d2d7"/>')
yy = sy + 6
# sparkline row: area chart + token text
sp = [6,8,7,10,12,11,14,17,15,19,23,21,26,30,28,25,29,33,31,28,26,30,27,24]
sp = [v*1000 for v in sp]
speak = max(max(sp), BURN)
gx, gw, gh2 = sx + 12, 88, 16
gb = yy + 18
svg.append(f'<rect x="{gx}" y="{gb}" width="{gw}" height="1" fill="#8e8e93" opacity="0.4"/>')
colw = gw / len(sp)
for i, v in enumerate(sp):
    r, g, b = gradient(v / BURN)
    hh = max(1, (v / speak) * gh2)
    x = gx + i * colw
    svg.append(f'<rect x="{x:.1f}" y="{gb-hh:.1f}" width="{colw:.1f}" height="1.6" fill="rgb({r},{g},{b})"/>')
    if hh > 1.6:
        svg.append(f'<rect x="{x:.1f}" y="{gb-hh+1.6:.1f}" width="{colw:.1f}" height="{hh-1.6:.1f}" fill="rgb({r},{g},{b})" opacity="0.38"/>')
svg.append(f'<text x="{gx+gw+10}" y="{gb-3}" fill="#1d1d1f" font-size="11.5">in 4k · out 8k · cw 2k</text>')
svg.append(f'<text x="{gx+gw+10}" y="{gb+9}" fill="#8e8e93" font-size="11">cr 180k · 12 msgs</text>')
yy += 26 + 6
subrows = [
    ("sep", ""),
    ("info", "running · opus-4-8 · cpu 34% · 420 MB"),
    ("info", "cwd ~/code/webapp"),
    ("info", "now · Edit …src/engine.rs · cargo 87% · node 12%"),
    ("sep", ""),
    ("act", "Pause (SIGSTOP)"),
    ("act", "Resume (SIGCONT)"),
    ("act", "Kill session…"),
]
for kind, label in subrows:
    cyy = yy + 16
    if kind == "sep":
        svg.append(f'<line x1="{sx+10}" y1="{yy+8}" x2="{sx+SW-10}" y2="{yy+8}" stroke="#e5e5ea"/>')
        yy += 12
        continue
    elif kind == "info":
        svg.append(f'<text x="{sx+14}" y="{cyy}" fill="#8e8e93" font-size="12">{label}</text>')
    else:
        color = "#d70015" if label.startswith("Kill") else "#1d1d1f"
        svg.append(f'<text x="{sx+14}" y="{cyy}" fill="{color}" font-size="13">{label}</text>')
    yy += 26

svg.append('</svg>')
open("docs/screenshot-menubar.svg", "w").write("\n".join(svg))
print("wrote docs/screenshot-menubar.svg")
