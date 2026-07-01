# Claude Code Observability TUI — Design

**Date:** 2026-07-01
**Status:** Approved (ready for implementation planning)

## Purpose

A terminal UI that surfaces **everything Claude Code is running** — sessions,
tasks, agents (including nested subagents), and watchers (hooks, `/loop` jobs,
scheduled routines, background commands) — across **local and remote** hosts.

The driving goal is **observability of resource and token consumption**: know
what is running, where tokens are going, and where they are *leaking* (runaway
loops, cache misses, agent storms). The tool is **observe + act**: it can stop a
burner the moment it is spotted.

## Non-goals (v1)

- No cost/dollar display. Raw token counts only (no `pricing.toml`).
- No lingering view of ended entities — **active entities only**.
- No remote monitoring in Phase 1 (SSH + cloud are Phase 2).
- No menu-bar app in Phase 1 (Phase 3), but the architecture must not preclude it.

## Form factor & stack

- **Terminal TUI** built in **Rust + ratatui**.
- Structured so a future **macOS menu-bar** client (`tray-icon` or Tauri) is a
  clean add-on reusing the same engine and IPC — not a rewrite.

## Architecture

A Cargo **workspace** splitting engine, daemon, and display clients:

```
ccwatch/
├── core/       library: data model, collectors, token accounting, leak
│               detection. Pure logic, no long-running I/O loop. Unit-testable.
├── daemon/     binary `ccwatchd`: owns FSEvents watchers + a poll timer, holds
│               live state + short rolling history, serves IPC. Uses core.
├── tui/        binary `ccwatch`: ratatui client. Connects to daemon, renders
│               snapshots, sends action requests. Auto-spawns daemon if absent.
└── menubar/    (Phase 3) tray-icon client. Same IPC, same daemon.
```

### Daemon (`ccwatchd`)

- On start, scans `~/.claude` once to build initial state, then registers
  **FSEvents watchers** on `sessions/`, `tasks/`, `projects/`, and settings
  files. File changes trigger incremental updates.
- Transcripts (`.jsonl`) are parsed **incrementally**: a per-file byte-offset
  watermark means only newly appended lines are read.
- A low-frequency **poll timer (~2s)** refreshes `ps` data (CPU/RSS) and
  re-checks `pid` liveness — these are not file events.
- Keeps **short rolling history** (token counts over time per session/agent) in
  memory plus a small on-disk ring buffer, so rate computation and leak trends
  survive a TUI restart or short daemon restart. (Display is active-only; history
  is internal, used to compute rates and detect leaks.)
- Writes a **pidfile** to avoid double-launch; optional idle-exit timeout (flag).

### IPC

- **Unix domain socket** at `~/.claude/ccwatch/daemon.sock`, newline-delimited
  JSON.
- Two message kinds:
  - `Subscribe` — daemon pushes a fresh `Snapshot` on every change, plus a
    heartbeat.
  - `Action` — client → daemon (e.g. terminate pid N); daemon executes and
    replies with the outcome.
- Same protocol serves the future menu-bar client.

### Lifecycle

- `ccwatch` (TUI) checks for the socket; if the daemon is not running it spawns
  `ccwatchd` detached, then connects.
- If the daemon dies, the TUI shows "daemon disconnected" and attempts re-spawn.

## Observable data sources (local)

| Source | Path | Provides |
|---|---|---|
| Sessions | `~/.claude/sessions/*.json` | `pid`, `sessionId`, `cwd`, `startedAt`, `kind`, `entrypoint`, `name`, `version` |
| Task lists | `~/.claude/tasks/<sessionId>/*.json` | `subject`, `status`, `blocks`/`blockedBy` |
| Transcripts | `~/.claude/projects/<slug>/<uuid>.jsonl` | per-message `usage` (input/output/cache-write/cache-read/server_tool_use), `model`, timestamps, tool calls (subagents via `Task`, background bash, `/loop`) |
| Hooks | `settings.json` (+ project/local/plugin settings) | configured event watchers |
| Processes | `ps` | live CPU/RSS per `claude` pid; cross-checked with session `pid` for liveness |

Cloud routines/agents are not on disk (API — Phase 2). SSH machines are read by
running the same probe against a remote `~/.claude` (Phase 2).

## Data model

Each entity is tagged with a `Host` so the TUI can group by machine.

- **`Host`** — `Local`, `Remote { name, ssh_target }`, or `Cloud`.
- **`Session`** — one running/recent Claude Code instance.
  - `id`, `name`, `cwd`, `pid`, `kind` (interactive/cloud/print), `entrypoint`,
    `version`, `model`
  - `state`: `Running` (pid alive, recent activity) / `Idle` (alive, no activity
    N min) / `Ended` (pid gone). **Only `Running`/`Idle` are displayed.**
  - `tokens`: `TokenLedger`; `resources`: `{ cpu_pct, rss_mb }`
  - links to its tasks, agents, watchers
- **`Task`** — a todo: `subject`, `status` (pending/in_progress/completed),
  `blocked_by`. Rolls up to per-session `done/total` counts.
- **`Agent`** — a subagent from transcript `Task`-tool calls: `subagent_type`,
  `description`, `state` (running/finished — inferred from whether a matching
  `tool_result` returned), `started_at`, attributed tokens, and **nested
  children** (an agent that spawns agents, recursively).
- **`Watcher`** — tagged enum, each with `last_fired` and, where knowable,
  attributed tokens:
  - `Hook { event, matcher, command }`
  - `Loop { interval, prompt, next_wake }`
  - `Routine { schedule, target }`
  - `BackgroundCmd { pid, command, state }`
- **`TokenLedger`** — per session, per agent, per model. Raw counters `input`,
  `output`, `cache_creation` (cw), `cache_read` (cr), `server_tool_use`. Derived
  sliding-window `tokens/min` (1m / 5m / session-total). Agent tokens roll up to
  the agent **and** its parent session.
- **`Alert`** — `{ severity, kind, subject_ref, message, since }`.

## Token accounting

- Fold each assistant message's `usage` into the relevant `TokenLedger`(s).
- Keep `input`, `output`, `cache_creation` (cw), `cache_read` (cr), and
  `server_tool_use` **separate** — never collapsed into one number.
- Derive `tokens/min` over sliding windows from timestamped rolling history.
- Attribution: tokens on messages within a subagent's span roll up to that agent
  and its ancestors, so nested workflows show own-burn and child-burn.

## Leak detection

Independent heuristics, each emitting an `Alert`. Thresholds ship as sensible
defaults, overridable via a config file.

1. **Runaway loop** — sustained high `tokens/min` with **no user turns** in the
   window (assistant talking to itself / a non-settling `/loop`).
2. **Cache-miss bleed** — `cache_read` collapses while `input` stays high →
   prompt cache not hit, re-paying full input repeatedly.
3. **Zombie session** — `pid` alive and still burning tokens while flagged idle
   or otherwise expected to be done.
4. **Agent storm** — many agents spawned in a short window, or one agent whose
   burn dwarfs its siblings.
5. **Stuck watcher** — a `/loop` or routine firing far more often than its
   interval, or a hook triggering in a tight cycle.

Each alert names the culprit entity and is actionable (jump to / act on it).

## TUI layout

Single-screen ratatui app, live-updating as the daemon pushes snapshots.

```
┌ ccwatch ──────────── local · 3 active · 128k tok/min · Σ 4.2M tok · cache 71% ──┐
│ ALERTS (2)                                                                       │
│  ⚠ runaway loop   webapp          62k tok/min · no user turn 7m · agent×3       │
│  ⚠ cache bleed    temp             cache-read 12%↓ · input 40k/min               │
├──────────────────────────────────────────────────────────────────────────────────┤
│ SESSIONS / AGENTS          model     state    up     tok/min  in/out/cw/cr   cpu  rss │
│ ▾ webapp        [int]     opus-4-8  running  2h14m   62k     40k/8k/2k/180k 34% 420M│
│    ├▾ Explore "search dir" general   running   0:40   0.4k    3k/0.5k/0/9k          │
│    │   └ general "sub-scan"          running   0:12   0.1k    ...                    │
│    └ Task "M2 bumps" in_progress                                                     │
│ ▾ ccwatch [int]    opus-4-8  running  0h08m    1k     14k/0.5k/3k/20k 6% 88M │
│ ▸ temp           [int]     opus-4-8  idle     1h02m     0     ...             0%  21M │
├───────────────────────────┬────────────────────────────────────────────────────────┤
│ TASKS · webapp (3/15)     │ WATCHERS · webapp                                       │
│  ● bump dependencies       │  loop   every 5m    /babysit-prs     next 2m10s  fired 8 │
│  ○ audit deploy  ⛌blocked  │  hook   Stop        notify.sh        fired 3            │
│  ✓ update changelog          │  bg     pid 8123    npm run dev       running 2h        │
├───────────────────────────┴────────────────────────────────────────────────────────┤
│ DETAILS · Explore "search dir"                                                        │
│  cwd /Users/demo/temp/ccwatch   parent webapp   started 14:22:07            │
│  tokens  in 3k · out 0.5k · cache-w 0 · cache-r 9k   msgs 12   last activity 3s ago   │
│  transcript projects/-Users-demo-temp/5cb3….jsonl                                     │
│  legend  in=input  out=output  cw=cache-write  cr=cache-read                          │
└───────────────────────────────────────────────────────────────────────────────────────┘
 /  jump   tab focus   enter expand   k kill   p pause   r resume   d details   f filter   q quit
```

- **Top bar**: host + live totals (active sessions, aggregate tokens/min,
  session-total `Σ`, aggregate cache-hit %).
- **Alerts pane**: leak-detector output, most severe first.
- **Sessions/agents tree**: expandable; nested agents as children (recursive)
  with per-agent token attribution. Columns: `model`, `state`, `uptime`,
  `tok/min`, `in/out/cw/cr`, `cpu`, `rss`.
- **Bottom split**: tasks for the selected session (status glyphs incl.
  blocked `⛌`, `done/total` header) left; that session's watchers (with
  `last_fired` / fire-count / `next_wake`) right.
- **Details pane**: follows selection — full cwd, parent, timestamps, exact token
  breakdown, message count, last-activity age, backing transcript path, and a
  legend for the `in/out/cw/cr` abbreviations.
- When remote (Phase 2) lands, hosts become collapsible top-level groups above
  sessions.

### Fuzzy jump (`/`)

Press `/` to open an fzf-style overlay. Typing any fragment matches across **all**
entities by name — session names, agent descriptions, task subjects, watcher
names/commands. Ranked incremental results; `enter` jumps selection to that item
(expanding ancestors as needed); `esc` cancels.

## Actions (observe + act)

The **daemon** executes all actions (so the future menu-bar client has the same
powers). Destructive actions require a confirm step showing exactly what is
affected, and every action is logged by the daemon.

| Key | Action | Effect |
|---|---|---|
| `k` on session | Kill session | `SIGTERM` the pid → grace period → `SIGKILL` if it survives |
| `k` on bg cmd | Kill background process | Terminate that background Bash pid, session stays alive |
| `p` / `r` | Pause / Resume | `SIGSTOP` / `SIGCONT` a session — freeze a burner without killing it |
| `x` | Disable hook | Remove/comment the hook entry in the relevant `settings.json` (with backup); reversible |
| `s` | Stop watcher | Best-effort stop for a `/loop` or routine (see limitation) |

**Limitation (stated honestly in the UI):** a `/loop` or subagent lives inside a
running Claude Code process — there is no external API to stop just that loop. So
for those the realistic actions are: **kill the owning session** (always works)
or **disable the hook/config** that spawns it (works for hooks and file-backed
schedules). The TUI clearly marks which actions are "surgical" vs. "kills the
whole session." Cloud routines (Phase 2) do have a per-routine cancel API.

## Error handling

- Every collector is fallible and isolated: a malformed `.json`, a half-written
  transcript line, a `ps` failure, or a vanished file never crashes the daemon;
  that collector logs and yields stale-but-valid data while the rest keep working.
- The TUI degrades gracefully if the daemon dies: shows "daemon disconnected" and
  attempts re-spawn.

## Testing

- `core` is pure logic over inputs — unit-tested against **fixture directories**
  (checked-in sample `~/.claude` trees + transcript files): parsing, token math,
  and each leak heuristic get deterministic tests.
- Daemon/IPC: a few integration tests (spawn, subscribe, receive snapshot, send
  action).
- TUI kept thin so most logic is testable without a terminal.

## Phasing

**Phase 1 — Local core (primary deliverable)**
- Workspace scaffold: `core`, `daemon`, `tui`.
- Collectors: sessions, tasks, transcripts (incremental), hooks, processes.
- Token ledger + rolling rates; the 5 leak heuristics → alerts.
- Daemon w/ FSEvents + poll timer, Unix-socket IPC, auto-spawn.
- TUI: alerts, session/agent tree, tasks + watchers, details pane, `/` fuzzy jump.
- Actions: kill / pause / resume / kill-bg / disable-hook, confirm + logging.

**Phase 2 — Remote**
- `Host` abstraction live. SSH adapter: probe/read remote `~/.claude`, merge its
  snapshot under a remote host group.
- Cloud adapter: authenticate to cloud API, list cloud agents/routines under a
  `Cloud` host, per-routine cancel.

**Phase 3 — Menu-bar app**
- `menubar` crate (`tray-icon`); connects to the same daemon socket; glanceable
  counts + alert badge, click to expand.

## Open items deferred to implementation planning

- Exact default thresholds for each leak heuristic.
- Snapshot/`Action` JSON schema specifics.
- On-disk rolling-history ring buffer format and retention window.
