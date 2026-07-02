# ccwatch

See everything Claude Code is doing — every session, agent, and process, on
this machine and your servers — and how fast it's burning through your token
budget.

![ccwatch terminal UI](docs/screenshot-tui.svg)

![ccwatch menu bar](docs/screenshot-menubar.svg)

## What it shows

- **Sessions** across all your machines: state, uptime, last activity, burn
  rate, token breakdown, cpu/ram
- **What each session is doing right now**: in-flight tool calls (editing
  which file, running which command) and the child processes it spawned
- **Agents** each session launched, including background ones, with live
  running/done state
- **The Governor** — a fuel gauge for your plan limits (see below)
- **Alerts** when something leaks: runaway loops, cache misses, agent storms,
  a session burning while idle, an unreachable server

And it acts: kill, pause, or resume a session from the TUI or the menu bar,
with confirmation.

## Install

Download the latest [release](https://github.com/jagajaga/ccwatch/releases)
(universal macOS binaries) or build from source:

```sh
cargo build --release
```

Then:

```sh
./target/release/ccwatch           # terminal UI
./target/release/ccwatch-menubar   # menu-bar app
```

Both start the background daemon automatically.

## Terminal UI

| Key | Action |
|---|---|
| `↑` `↓` / `enter` | move / expand session's agents |
| `/` | fuzzy-jump to anything by name |
| `d` | details popup |
| `s` | cycle sort (tok/min, last active, name, cpu, rss) |
| `f` / `x` | hide idle sessions / hide finished agents |
| `k` / `p` / `r` | kill / pause / resume (with confirmation) |
| `q` | quit |

## Menu bar

A live burn graph sits in the menu bar (teal = calm, red = at your limit),
next to a readout you choose in **Settings**: throttle, burn rate, range,
tank %, or graph only. Click for the dropdown: governor line, alerts, and one
submenu per session with its burn sparkline, current activity, and
kill/pause/resume. Preferences persist across restarts.

## The Governor

Like a car's range estimate: floor it and you hit your plan limit early; ease
off and you coast to the reset.

- **Tank** — how much of your 5-hour plan window is left. The window anchors
  the way Anthropic's does (first message starts it), and the budget is
  **learned automatically from the 429 rate-limits in your own history** — no
  configuration needed (a `~` marks it as an estimate; set `window_budget` to
  override).
- **Throttle** (`▲2.1×` / `▼0.6×`) — how your current burn compares to the
  pace that would land exactly at the reset. Above 1× means you'll hit the
  wall first — and a "limit ahead" alert tells you when.
- **Range** — minutes until empty at the current speed.

Burn counts what you actually pay for (input + output + cache writes, across
every machine); cheap cache reads are tracked separately.

## Remote machines

```json
// ~/.claude/ccwatch/remotes.json
[{ "name": "my-server", "kind": "ssh", "target": "user@host" }]
```

Nothing to install remotely: the daemon pipes a self-contained Python probe
over ssh, which reads the remote `~/.claude` in place and reports back.
Needs key-based ssh and python3, that's all. Remote sessions appear next to
local ones — killable too (TERMs the pid over ssh). If a server stops
responding you get an alert instead of silence.

## Configuration

Optional, in `~/.claude/ccwatch/config.toml`:

```toml
hourly_budget = 3_000_000     # self-imposed cruise budget, tokens/hour
#window_budget = 200_000_000  # plan-window budget; unset → learned from 429s
terminal = "iTerm"            # for "Open TUI dashboard"; unset → auto-detect
burn_tokens_per_min = 40000   # when the graph turns red
```

## How it works

```
~/.claude (sessions, transcripts, tasks)     ssh → remote ~/.claude
                    │                                │
                    ▼                                ▼
                ccwatchd ──────────────── merges everything, computes
                    │                     rates, alerts, the Governor
        unix socket │ (JSON snapshots)
          ┌─────────┴─────────┐
          ▼                   ▼
       ccwatch          ccwatch-menubar
```

One daemon does all the work: it tails Claude Code's own transcript files
(incrementally — only new bytes), watches the process table, and pushes a
full snapshot to any connected client. The TUI and menu bar are thin views
over the same data. Everything is local; nothing is sent anywhere except
your own ssh connections.

Full design notes live in
[docs/superpowers/specs](docs/superpowers/specs/2026-07-01-claude-code-observability-tui-design.md).
