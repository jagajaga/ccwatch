<p align="center"><img src="assets/icon.svg" width="140" alt="ccwatch"></p>

<h1 align="center">ccwatch</h1>

<p align="center">
Mission control for Claude Code.<br/>
Every session, every agent, every token — every machine.
</p>

<p align="center">
<a href="https://github.com/jagajaga/ccwatch/actions/workflows/ci.yml"><img src="https://github.com/jagajaga/ccwatch/actions/workflows/ci.yml/badge.svg" alt="ci"></a>
<a href="https://github.com/jagajaga/ccwatch/releases"><img src="https://img.shields.io/badge/macOS-universal-black" alt="macos"></a>
</p>

---

Claude Code will happily burn your entire 5-hour window while you're at lunch.
Sessions pile up, agents spawn agents, a server grinds all night — and the
first you hear is **"limit reached, resets at 03:00."**

ccwatch already knows. It was watching.

![ccwatch terminal UI](docs/screenshot-tui.svg)

![ccwatch menu bar](docs/screenshot-menubar.svg)

## Features

- ⛽ **The Governor** — a fuel gauge that **learns your real plan limit from
  your own 429s** — and re-measures it on every confirmed wall, so plan
  upgrades *and downgrades* self-correct. `▲2.1×` = hitting the wall 40 min
  before reset. `▼0.6×` = coast home. Zero config.
- 🖥 **Fleet view** — all sessions, all machines: burn rate, tokens, cpu/ram,
  last activity, the same titles Claude's UI uses
- 🔍 **Live activity** — `✎ Edit engine.rs` · `⚙ cargo build 87%` — in-flight
  tool calls *and* real child processes, per session
- 🤖 **Agents** — who spawned what, nested, with truthful running/done state
  (background agents included)
- 🚨 **Leak alerts** — runaway loops, cache bleed, agent storms, sessions
  burning while "idle", servers gone dark
- 🔪 **Kill switch** — kill / pause / resume from menu bar or terminal,
  always with confirmation
- 🛰 **Remote machines, zero install** — one line of JSON; a python probe
  goes over ssh, nothing gets installed

## Install

**Menu-bar app** — grab `ccwatch-menubar-*.zip` from the
[latest release](https://github.com/jagajaga/ccwatch/releases/latest), unzip,
drop `ccwatch-menubar.app` into `/Applications`, open it. The binaries are
unsigned, so the first launch needs one of:

```sh
xattr -dr com.apple.quarantine /Applications/ccwatch-menubar.app
```

(or right-click → Open → Open). Then **Settings ▸ Start at login** makes it
permanent.

**Terminal UI** — grab `ccwatch-*-macos-universal.tar.gz` from the same
release:

```sh
tar xzf ccwatch-*-macos-universal.tar.gz
mv ccwatch/ccwatch ccwatch/ccwatchd /usr/local/bin/   # or anywhere on PATH
ccwatch
```

**From source** (any of it):

```sh
cargo build --release && ./target/release/ccwatch
```

No setup. No accounts. No telemetry. The daemon starts itself and exits
itself when the last window closes.

## Drive

| | | | |
|---|---|---|---|
| `/` jump anywhere | `d` details | `s` sort | `enter` expand |
| `k` kill | `p` `r` pause/resume | `f` hide idle | `x` hide done |

Menu bar → **Settings**: pick what sits next to the graph (throttle, burn
rate, range, tank %, or nothing), hide idle sessions, vanish from the bar
entirely while nothing is running (it returns by itself), and **start at
login**.

## Remote machines

```json
// ~/.claude/ccwatch/remotes.json
[{ "name": "my-server", "kind": "ssh", "target": "user@host" }]
```

Needs ssh keys and python3 on the box. That's the whole setup. Remote
sessions land in the same views, killable over ssh, and a dead server
becomes an alert — not a silent gap.

## Tune

```toml
# ~/.claude/ccwatch/config.toml — all optional
hourly_budget = 3_000_000     # personal cruise budget, tokens/hour
#window_budget = 200_000_000  # plan window; unset → learned from 429s
terminal = "iTerm"            # for "Open TUI dashboard"
burn_tokens_per_min = 40000   # where the graph turns red
```

## Under the hood

```
~/.claude  ──┐                      ┌── ssh ── remote ~/.claude
             ▼                      ▼
          ccwatchd ── tails transcripts (new bytes only), watches pids,
             │        computes rates · alerts · the Governor
             ├────────────┐   unix socket, JSON snapshots
             ▼            ▼
          ccwatch    ccwatch-menubar
```

Claude Code already writes everything worth knowing into `~/.claude`.
ccwatch just reads it well. Idle cost: ~0% CPU.
