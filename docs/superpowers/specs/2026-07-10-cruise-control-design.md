# Cruise Control — Design

Status: draft · 2026-07-10

## Summary

**Cruise Control** is the enforcement arm of the Governor. The Governor already
*measures* plan usage (the 5-hour and weekly tanks, the cruise pace, the
throttle, the wall projection). Cruise Control *acts* on those numbers: it holds
the fleet's token burn at a target pace so a plan window is used fully but never
runs out early — by throttling **background/autonomous** work (workflow agent
fleets, `/loop`s, overnight servers), never the session the user is actively
typing in.

It borrows directly from network QoS, congestion control, and ad-budget pacing:
a target rate (bandwidth control), priority tiers (DSCP), and — at its core — a
single **dual-price controller** (Kelly's Network Utility Maximization; the
regret-optimal dual-mirror-descent method from budget pacing) that paces the
fleet *and* shares the budget fairly at once, converging to proportional
fairness. An **AIMD** panic-brake handles the 429 shock (TCP congestion control).

## Motivation

Redline was built around one failure: "Claude Code burns your entire 5-hour
window while you're at lunch." Today Redline can *see* that happening (the
Governor throttle goes `▲1.9×`) and can *manually* Kill/Pause/Resume, but the
user has to notice and act. Cruise Control closes the loop: it turns the throttle
gauge into a thermostat.

## Goals

- Keep the **binding** tank (5h or weekly, whichever we already compute in Mix)
  from hitting its wall before its reset, using the full budget otherwise.
- Support an explicit **reservation/deadline** ("make weekly last past Friday",
  "reserve 15M tokens for a 6pm run") as the same control loop with a different
  setpoint.
- Act on **background/autonomous burn only**; the foreground interactive session
  is never touched.
- Ship in three trust tiers — **Advisory → One-click → Autonomous** — sharing one
  policy layer.
- Every automated action is visible, logged, and instantly reversible.

## Non-goals

- Fine-grained per-request shaping. Redline observes `~/.claude`; it does not
  proxy the Anthropic API, so it cannot inject per-request delays. The actuator
  is pause/resume of processes (whole sessions and, preferentially, individual
  fleet agents).
- Changing what Claude Code does. We only start/stop OS processes it already
  spawned.
- Cross-machine coordination beyond what the remote probe already provides.

## Concepts

### Setpoint — coast and reservation, unified

For the binding tank:

```
target_rate = max(0, (budget_remaining − reserve)) / minutes_to(deadline)
```

- **Coast (default):** `reserve = 0`, `deadline = tank.resets_at`. This is
  exactly the Governor's existing `cruise_per_min`; holding burn ≤ it keeps the
  throttle ≤ 1×.
- **Reservation:** `reserve = R` tokens to leave in the tank at `deadline`.
- **Deadline:** substitute an earlier `deadline` to stretch the budget to a
  chosen time.

The setpoint is recomputed every snapshot (budgets/resets already refresh there).
All figures are billable Opus-equivalent tokens, consistent with the tanks.

### Priority tiers

`High` · `Normal` · `Background`. Inferred by default, override-able:

- **High / exempt:** the foreground interactive session (see Detection). Never
  paused by Cruise Control.
- **Background:** `entrypoint` is a loop/workflow/remote/headless context, or the
  session has been idle-of-user-input while its agents burn (the "at lunch"
  case). These are the throttle targets.
- **Normal:** everything else; throttled only after Background is exhausted and
  only in One-click/Autonomous with explicit opt-in.

Overrides live in config (`cruise.priority` by session name/cwd) and as a
right-click / key action in the UI.

### Fairness — proportional, via one price

Pacing and fairness are the *same* problem: maximise useful work subject to
`Σ burn ≤ target_rate`. Kelly's Network Utility Maximization solves it with a
single scalar **pace price** `λ` (the Lagrange multiplier on the budget). Each
eligible session's allowed burn is `weight / λ` (priority raises `weight`), so:

- raising `λ` throttles everyone, the heaviest / lowest-priority most;
- idle sessions draw ≈0 and their budget flows to active ones automatically — no
  explicit redistribution step;
- the fixed point is **proportional fairness** — the principled objective WFQ
  only approximates.

One scalar replaces both a per-session fair-queuing scheduler and a separate
pacing loop.

## The actuator

Ordered from finest to coarsest; the planner always prefers the finest that
achieves the target:

1. **Fleet concurrency (primary).** Workflow runs spawn dozens–hundreds of
   parallel background agents (observed: 171 live in one run). Pausing a
   *fraction* of a fleet's agent processes dials its burn down smoothly — this is
   adaptive concurrency limiting / probabilistic throttling, not a blunt on/off.
   Prefer pausing an agent that is *between turns* (no in-flight tool call) to
   avoid killing a live request.
2. **Whole Background session pause.** When a session has no fleet to thin (a
   plain `/loop` or a single long agent), pause the session process.
3. **Never the foreground.** High/exempt sessions are removed from the candidate
   set before planning.

Mechanism is the existing `pause`/`resume` actions (SIGSTOP/SIGCONT) applied to
the chosen pids; no new privilege.

## The control loop

Runs in the daemon (Autonomous mode) and is *computed* (but not applied) in
Advisory/One-click so the UI can show the plan.

### Normal regime — dual-price (mirror descent)

One state variable, the pace price `λ`, updated each snapshot by dual gradient
ascent:

```
λ ← max(0, λ + η · (actual_burn − target_rate))
```

- `λ` rises when over target, falls when under — a smooth, self-tuning integral
  controller with a single step-size `η`. No three PID gains, and no integral
  windup: when the actuator *saturates* (only the exempt foreground is left),
  `λ` simply settles higher and the planner takes no illegal action.
- The price maps to an **allowed concurrency per fleet**: `permits =
  round(weight / λ)` agents may run; the planner pauses the rest (preferring
  agents *between* turns). This quantises the continuous price onto the discrete
  actuator cleanly.
- A **dead-band** on `permits` changes plus a minimum dwell time between actions
  prevents pause/resume flapping.

### 429 regime — AIMD panic-brake

On a fresh 429 (already surfaced as `rate_limits`): multiplicative cut — pause a
large fraction of Background concurrency immediately — then additive ramp-back
once burn is under target and no new 429s arrive. This is the TCP AIMD response
and protects against a wall that the smooth loop is too slow for.

## Progressive rollout (three modes, one policy)

The planner (`plan(snapshot, config, state) -> (PacingPlan, PacerState)`) is pure
and shared. Modes differ only in what happens to the plan:

1. **Advisory.** Render it: a burn-down chart (actual vs. the target trajectory),
   the current `target_rate`, and the concrete recommendation ("2.1× over — pause
   ~40 of fleet `score_v3_holdout` to coast"). User acts manually.
2. **One-click.** The recommendation becomes buttons; plus a **"Cruise to reset"**
   toggle (and a reservation/deadline picker) that applies the plan on Background
   tier only, with a visible action log.
3. **Autonomous.** The daemon applies the plan continuously, per-tier opt-in,
   foreground always exempt, with a one-key global override ("release") and a log
   of every pause/resume with its reason.

## Architecture

- **`core/src/pacer.rs` (new).** Pure policy: `target_rate`, tiering, and the
  dual-price controller. `plan(snapshot, config, prev_state) -> (PacingPlan,
  PacerState)`, where `PacerState` is the scalar price `λ` plus AIMD bookkeeping,
  threaded in and out. Deterministic, no wall clock inside (`now_ms` is an
  argument), unit-testable with fixture snapshots like `governor.rs`. The plan
  carries `actions: Vec<PaceAction>`, `target_rate`, `price`, per-fleet `permits`,
  and a human `reason`.
- **`daemon`.** In Autonomous mode a loop calls `plan(...)` each refresh and
  executes `PaceAction`s via the existing action path; writes an action-log line
  per change. Advisory/One-click compute the plan and ship it in the snapshot.
- **`core/src/model.rs`.** `Snapshot` gains an optional `pacing: Option<PacingPlan>`
  (advisory data + current mode); `Session`/`Agent` gain a `priority` and a
  `paced` flag (so the UI can badge paced work).
- **`core/src/ipc.rs`.** New `ClientMsg` actions: `SetCruiseMode`, `SetReservation`,
  `SetPriority`, `ReleaseAll`. Snapshot already carries the plan.
- **`app` (SwiftUI) + `tui`.** Governor header gains a Cruise Control control:
  mode segmented control, the burn-down chart, the recommendation/buttons, the
  reservation picker, and the action log. Paced sessions/agents get a badge.
- **`core/src/config.rs`.** `[cruise]` block: `mode` (off/advisory/oneclick/auto),
  `reserve`, `deadline`, per-session `priority` overrides, hysteresis knobs
  (`band`, `dwell_secs`), `aimd_cut`. All optional with sane defaults.

## Detection details

- **Foreground / High:** the session whose `entrypoint` is interactive
  (`claude-desktop`, `claude-vscode`, `cli`) AND has the most recent *user* turn
  (`last_user_turn`), local host. Ties → most recent. Falls back to "exempt all
  interactive-entrypoint sessions" if ambiguous — conservative (never auto-pause
  something that might be foreground).
- **Background inference:** loop/workflow/remote entrypoints, or a session whose
  own `last_user_turn` is stale while its agents burn.
- All inference is override-able and shown in the UI so the user can correct it.

## Risks & mitigations

- **Coarse actuator** → fleet-concurrency granularity gives many small knobs;
  whole-session pause only as fallback.
- **In-flight request timeout when paused** → prefer pausing agents between turns
  (no pending tool call); accept that a paused agent may drop a live request and
  retry on resume (Claude Code already tolerates this on SIGCONT in practice —
  **must be validated**).
- **Flapping** → hysteresis band + minimum dwell time + additive resume.
- **Pausing the wrong (foreground) thing** → conservative exemption; foreground
  removed from candidates before planning; global "release" hotkey.
- **User confusion about "why did my agent stop?"** → every action logged with a
  human reason, paced work badged, one-key release.

## Testing

- `pacer.rs` unit tests over fixture snapshots (like `governor`/`engine` tests):
  coast setpoint math; reservation/deadline setpoint; the price loop converges so
  burn settles at target; proportional fairness (double a session's weight → ≈2×
  its permits); idle sessions draw ≈0; saturation (only foreground left → `λ`
  rises, no action, no windup); dead-band (no flap on small changes); AIMD cut on
  429 then additive ramp; foreground never appears in the plan.
- Daemon integration test: Autonomous mode applies and reverses actions against a
  fixture fleet; action log written.
- Manual end-to-end (verify skill): drive a real background workflow over target
  and confirm Cruise Control brings the projected burn under cruise without
  touching the foreground session, then releases cleanly.

## Rollout / sequencing

1. `pacer.rs` + setpoint + tiering + fair-share + plan, fully unit-tested. Snapshot
   carries the plan. (No enforcement yet.)
2. **Advisory** UI (chart + recommendation) in app and TUI.
3. **One-click** (buttons + "Cruise to reset" toggle + reservation picker + log).
4. **Autonomous** loop in the daemon behind explicit per-tier opt-in, with AIMD.

Each step ships independently and is useful on its own.

## Open questions

- Does a SIGSTOP'd Claude Code agent cleanly resume a dropped in-flight request on
  SIGCONT, or does it error the turn? Determines whether "pause between turns" is a
  nicety or a hard requirement. **Validate before Autonomous.**
- Reservation/deadline UI: minimal (one reserve field + one datetime) for v1;
  richer "profiles" later.

## Prior art

- Router SQM/AQM: fq_codel & CAKE, HTB rate limiting — bufferbloat.net; "Piece of
  CAKE" (arXiv 1804.07617).
- Fair queuing: WFQ / Deficit Round Robin — Wikipedia "Fair queuing"; intronetworks
  ch. 23.
- Congestion control as rate limiting: AIMD (Chiu–Jain fairness/efficiency
  convergence), Netflix concurrency-limits, ThomWright/congestion-limiter.
- **The unifying theory (the core algorithm):** Kelly's Network Utility
  Maximization — the price/dual decomposition that TCP distributedly approximates;
  Balseiro–Lu–Mirrokni, "Dual Mirror Descent for Online Allocation Problems"
  (regret-optimal budget pacing — the same λ update).
- Ad budget pacing: probabilistic throttling + PID (the pragmatic baseline we are
  improving on) — arXiv 2503.06942 ("A Practical Guide to Budget Pacing
  Algorithms"); "Feedback Control for Small Budget Pacing" (arXiv 2509.25429).
- LLM fleets: token-based (not request-based) limiting, multi-window budgets,
  per-agent buckets, burn-rate auto-throttle — TrueFoundry / Zuplo / AI Security
  Gateway write-ups.
