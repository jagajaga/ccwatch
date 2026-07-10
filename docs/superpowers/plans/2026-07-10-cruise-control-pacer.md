# Cruise Control — Pacer Policy Layer (Step 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the pure, deterministic Cruise Control policy layer (`core/src/pacer.rs`) that turns a Governor snapshot into a pacing plan via a dual-price (NUM/Kelly) controller, expose it in the daemon snapshot as advisory data, and parse its config — with no enforcement yet.

**Architecture:** A pure function `plan(snapshot, config, prev_state, now_ms) -> (PacingPlan, PacerState)` computes a target burn rate for the binding tank, infers per-session priority, runs one scalar pace-price `λ` (dual gradient ascent, AIMD on 429), and emits `PaceAction`s that pause/resume whole Background sessions to hold burn at target. The foreground interactive session is always exempt. The daemon computes this each refresh and attaches it to the `Snapshot`; nothing is executed.

**Tech Stack:** Rust (workspace crate `ccwatch-core`), serde, existing `Snapshot`/`Config` types, `cargo test`.

## Global Constraints

- Rust edition 2021; workspace crates `core` (`ccwatch-core`), `daemon`, `tui`.
- All token figures are **billable Opus-equivalent tokens** (match the Governor tanks). Burn rate is billable tok/min over the rate window.
- `pacer.rs` is **pure**: no wall clock inside (`now_ms: i64` is always an argument), no I/O. Same discipline as `governor.rs`.
- Clippy runs with `-D warnings` in CI (`cargo clippy --workspace --all-targets -- -D warnings`) — plan code must be clippy-clean.
- Wire types added to `model.rs` must derive `Clone, Debug, Serialize, Deserialize` and use `#[serde(rename_all = "snake_case")]` on enums, matching existing model types.
- Foreground/High priority sessions are NEVER emitted in a `Pause` action.
- Commit after every task with a `feat:`/`test:` message.

---

### Task 1: Setpoint — `target_rate`

The control setpoint: the billable tok/min that spends `remaining − reserve`
evenly over the time to the deadline. Coast = `reserve 0`, `deadline = reset`.

**Files:**
- Create: `core/src/pacer.rs`
- Modify: `core/src/lib.rs` (add `pub mod pacer;`)

**Interfaces:**
- Produces: `pub fn target_rate(remaining: u64, reserve: u64, mins_to_deadline: f64) -> f64`

- [ ] **Step 1: Register the module**

In `core/src/lib.rs`, add alongside the other `pub mod` lines:

```rust
pub mod pacer;
```

- [ ] **Step 2: Write the failing test**

Create `core/src/pacer.rs` with:

```rust
//! Cruise Control — the Governor's enforcement policy. Pure and deterministic:
//! a snapshot in, a pacing plan out. A single dual-price controller paces the
//! fleet's burn and shares the budget fairly at once (Kelly NUM / dual mirror
//! descent); an AIMD overlay handles the 429 shock. No enforcement here.

/// Billable tok/min that spends `remaining - reserve` evenly until the deadline.
/// `reserve == 0` and `deadline == reset` gives the Governor's coast pace.
pub fn target_rate(remaining: u64, reserve: u64, mins_to_deadline: f64) -> f64 {
    if mins_to_deadline <= 0.0 {
        return 0.0;
    }
    let spendable = remaining.saturating_sub(reserve) as f64;
    spendable / mins_to_deadline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coast_spends_all_remaining_over_the_window() {
        // 120M remaining, no reserve, 200 min to reset → 600k/min.
        assert_eq!(target_rate(120_000_000, 0, 200.0), 600_000.0);
    }

    #[test]
    fn reservation_lowers_the_target() {
        // Keep 20M in reserve → only 100M is spendable → 500k/min.
        assert_eq!(target_rate(120_000_000, 20_000_000, 200.0), 500_000.0);
    }

    #[test]
    fn past_deadline_or_over_reserve_is_zero() {
        assert_eq!(target_rate(120_000_000, 0, 0.0), 0.0);
        assert_eq!(target_rate(10_000_000, 20_000_000, 200.0), 0.0);
    }
}
```

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p ccwatch-core pacer::tests::coast_spends_all_remaining_over_the_window pacer::tests::reservation_lowers_the_target pacer::tests::past_deadline_or_over_reserve_is_zero`
Expected: 3 passed. (Implementation was written with the test; this task is small enough that test + impl land together.)

- [ ] **Step 4: Commit**

```bash
git add core/src/lib.rs core/src/pacer.rs
git commit -m "feat(pacer): target_rate setpoint (coast + reservation)"
```

---

### Task 2: Priority inference

Classify a session `High` (foreground, exempt) / `Normal` / `Background`.
Wire enum lives in `model.rs` (it will ride in the snapshot later).

**Files:**
- Modify: `core/src/model.rs` (add `Priority`)
- Modify: `core/src/pacer.rs` (add `priority_of`)

**Interfaces:**
- Consumes: `Session` fields `entrypoint: String`, `last_user_turn: Option<i64>` (verify these exist on `Session`; `last_user_turn` may be named differently — if the field is absent, use `last_activity: Option<i64>` and adjust the test accordingly).
- Produces:
  - `model.rs`: `pub enum Priority { High, Normal, Background }`
  - `pacer.rs`: `pub fn priority_of(entrypoint: &str, last_user_turn: Option<i64>, now_ms: i64, idle_secs: i64) -> Priority`

- [ ] **Step 1: Add the wire enum**

In `core/src/model.rs`, near `AgentState`:

```rust
/// Cruise Control priority tier. `High` is the foreground session — never paced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    High,
    Normal,
    Background,
}
```

- [ ] **Step 2: Write the failing test**

Append to `core/src/pacer.rs` tests module:

```rust
    use ccwatch_core_priority_import::Priority; // replaced below

    #[test]
    fn interactive_with_recent_user_turn_is_high() {
        let now = 1_000_000_000;
        // claude-vscode, user typed 10s ago → foreground → High/exempt.
        assert_eq!(priority_of("claude-vscode", Some(now - 10_000), now, 120), Priority::High);
    }

    #[test]
    fn loop_and_workflow_entrypoints_are_background() {
        let now = 1_000_000_000;
        assert_eq!(priority_of("loop", Some(now - 10_000), now, 120), Priority::Background);
        assert_eq!(priority_of("workflow", None, now, 120), Priority::Background);
    }

    #[test]
    fn interactive_but_idle_of_user_is_background() {
        let now = 1_000_000_000;
        // Interactive entrypoint but no user turn for 10 min → the "at lunch"
        // case → Background.
        assert_eq!(priority_of("claude-desktop", Some(now - 600_000), now, 120), Priority::Background);
    }
```

Fix the import line: replace `use ccwatch_core_priority_import::Priority;` with `use crate::model::Priority;` at the top of the `tests` module (add it next to `use super::*;`).

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p ccwatch-core pacer::tests::interactive_with_recent_user_turn_is_high`
Expected: FAIL — `priority_of` not found.

- [ ] **Step 4: Implement**

Add to `core/src/pacer.rs` (above the tests module):

```rust
use crate::model::Priority;

/// Interactive entrypoints. A session on one of these, with a recent user turn,
/// is the foreground and is exempt.
const INTERACTIVE: [&str; 3] = ["claude-desktop", "claude-vscode", "cli"];

/// Infer a session's tier. Foreground (interactive entrypoint + a user turn
/// within `idle_secs`) is `High`; loops/workflows/remote or interactive-but-idle
/// sessions are `Background`; everything else is `Normal`.
pub fn priority_of(
    entrypoint: &str,
    last_user_turn: Option<i64>,
    now_ms: i64,
    idle_secs: i64,
) -> Priority {
    let interactive = INTERACTIVE.contains(&entrypoint);
    let user_recent = last_user_turn.is_some_and(|t| now_ms - t <= idle_secs * 1000);
    if interactive && user_recent {
        Priority::High
    } else if !interactive || !user_recent {
        Priority::Background
    } else {
        Priority::Normal
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ccwatch-core pacer::tests::`
Expected: all Task-1 and Task-2 tests pass.

- [ ] **Step 6: Verify the `Session` field name**

Run: `grep -n "last_user_turn\|last_activity" core/src/model.rs`
If `last_user_turn` is not a public field on `Session`, note that Task 5 must pass `session.last_activity` into `priority_of` instead. (`priority_of` itself is agnostic — it takes an `Option<i64>`.)

- [ ] **Step 7: Commit**

```bash
git add core/src/model.rs core/src/pacer.rs
git commit -m "feat(pacer): priority inference (foreground exempt, background paced)"
```

---

### Task 3: Dual-price controller + allowed-burn

The core algorithm: update the scalar price `λ` by dual gradient ascent, and map
a session's weight+price to its allowed burn (`weight / λ`).

**Files:**
- Modify: `core/src/pacer.rs`

**Interfaces:**
- Produces:
  - `pub fn update_price(prev: f64, actual_burn: f64, target_rate: f64, eta: f64) -> f64`
  - `pub fn allowed_burn(weight: f64, price: f64) -> f64`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module:

```rust
    #[test]
    fn price_rises_over_target_and_falls_under() {
        // Over target → price up.
        let up = update_price(1.0, 800_000.0, 600_000.0, 1e-6);
        assert!(up > 1.0, "over target should raise price, got {up}");
        // Under target → price down.
        let down = update_price(1.0, 400_000.0, 600_000.0, 1e-6);
        assert!(down < 1.0, "under target should lower price, got {down}");
    }

    #[test]
    fn price_never_negative() {
        // Massively under target must clamp at 0, not go negative.
        assert_eq!(update_price(0.0, 0.0, 600_000.0, 1e-3), 0.0);
    }

    #[test]
    fn allowed_burn_is_proportional_to_weight() {
        // Double the weight → double the allowed burn at the same price.
        assert_eq!(allowed_burn(2.0, 0.5), 2.0 * allowed_burn(1.0, 0.5));
    }

    #[test]
    fn loop_converges_to_target() {
        // Simulate: each tick, everyone burns their allowed share; price should
        // drive total burn toward target. One unit, weight 1.
        let target = 600_000.0;
        let mut price = 1e-6;
        let eta = 1e-9;
        let mut burn = 2_000_000.0; // start way over
        for _ in 0..2000 {
            price = update_price(price, burn, target, eta);
            burn = allowed_burn(1.0, price); // the unit obeys its permit
        }
        assert!((burn - target).abs() / target < 0.05, "converged near target: {burn}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ccwatch-core pacer::tests::price_rises_over_target_and_falls_under`
Expected: FAIL — `update_price` not found.

- [ ] **Step 3: Implement**

Add to `core/src/pacer.rs`:

```rust
/// Dual gradient ascent on the budget constraint: the pace price `λ` rises when
/// burn is over target and falls when under. `eta` is the step size. Clamped at
/// zero (a non-binding budget has price 0). This is the whole pacing loop.
pub fn update_price(prev: f64, actual_burn: f64, target_rate: f64, eta: f64) -> f64 {
    (prev + eta * (actual_burn - target_rate)).max(0.0)
}

/// A unit's allowed burn under the current price: `weight / λ`. Higher price
/// throttles everyone; higher weight (priority) buys a bigger share. Price 0
/// means the budget isn't binding → unbounded (represented as f64::INFINITY).
pub fn allowed_burn(weight: f64, price: f64) -> f64 {
    if price <= 0.0 {
        f64::INFINITY
    } else {
        weight / price
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ccwatch-core pacer::tests::`
Expected: all pass, including `loop_converges_to_target`.

- [ ] **Step 5: Commit**

```bash
git add core/src/pacer.rs
git commit -m "feat(pacer): dual-price controller + weighted allowed-burn (NUM/Kelly)"
```

---

### Task 4: AIMD 429 overlay

A fresh 429 is a hard signal the smooth loop is too slow for: multiplicatively
jump the price. This is the only non-gradient move.

**Files:**
- Modify: `core/src/pacer.rs`

**Interfaces:**
- Produces: `pub fn aimd_on_429(price: f64, cut: f64) -> f64`

- [ ] **Step 1: Write the failing test**

Append to the `tests` module:

```rust
    #[test]
    fn aimd_multiplicatively_jumps_the_price() {
        // A 429 with cut factor 4 quadruples the price (a hard brake). From a
        // zero/near-zero price it still produces a meaningfully positive price.
        assert_eq!(aimd_on_429(2.0, 4.0), 8.0);
        assert!(aimd_on_429(0.0, 4.0) > 0.0, "must brake even from price 0");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ccwatch-core pacer::tests::aimd_multiplicatively_jumps_the_price`
Expected: FAIL — `aimd_on_429` not found.

- [ ] **Step 3: Implement**

Add to `core/src/pacer.rs`:

```rust
/// Smallest non-zero price the brake floors to, so an AIMD cut from price 0 still
/// throttles hard instead of multiplying zero.
const AIMD_FLOOR: f64 = 1e-6;

/// AIMD multiplicative-increase on a fresh 429: jump the price by `cut`×. From a
/// zero price, floor first so the brake actually bites.
pub fn aimd_on_429(price: f64, cut: f64) -> f64 {
    price.max(AIMD_FLOOR) * cut
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ccwatch-core pacer::tests::aimd_multiplicatively_jumps_the_price`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/pacer.rs
git commit -m "feat(pacer): AIMD price brake on 429"
```

---

### Task 5: Wire types + `plan()` integration

Bring it together: `PacerState` (threaded λ), `PaceAction`, `PacingPlan`, and the
integration `plan()` that reads a snapshot, computes target + price, and emits
pause/resume actions on Background sessions — foreground always exempt.

**Files:**
- Modify: `core/src/model.rs` (add `PaceAction`, `PacingPlan`)
- Modify: `core/src/pacer.rs` (add `PacerState`, `PacerConfig`, `plan`)

**Interfaces:**
- Consumes: `Snapshot { sessions: Vec<Session>, governor: Option<GovernorStatus> }`; `GovernorStatus::binding() -> (&Tank, bool)`; `Tank { used, budget: Option<u64>, resets_at: Option<i64>, rate_per_min }`; `Session { pid: Option<i32>, entrypoint, last_activity, tokens_per_min, name, agents: Vec<Agent> }`; `Agent { subagent_type: String, description: String, children: Vec<Agent> }` — used to detect a **fleet-session** (a session whose agents include a `Workflow` node with children) and to count/label it.
- Produces:
  - `model.rs`: `pub enum PaceAction { Pause { pid: i32, reason: String }, Resume { pid: i32 } }`, `pub struct PacingPlan { pub target_rate: f64, pub actual_rate: f64, pub price: f64, pub actions: Vec<PaceAction>, pub reason: String }`
  - `pacer.rs`: `pub struct PacerState { pub price: f64 }`, `pub struct PacerConfig { pub reserve: u64, pub deadline_ms: Option<i64>, pub eta: f64, pub aimd_cut: f64, pub idle_secs: i64, pub dead_band: f64 }` with `Default`, and `pub fn plan(snap: &Snapshot, cfg: &PacerConfig, prev: PacerState, now_ms: i64, saw_429: bool) -> (PacingPlan, PacerState)`

- [ ] **Step 1: Add wire types to `model.rs`**

In `core/src/model.rs`:

```rust
/// A Cruise Control action on a session process. Advisory in Step 1 (computed,
/// not executed).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PaceAction {
    Pause { pid: i32, reason: String },
    Resume { pid: i32 },
}

/// The pacing plan for one snapshot: the target burn, the current burn, the pace
/// price, and the actions that would hold burn at target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PacingPlan {
    pub target_rate: f64,
    pub actual_rate: f64,
    pub price: f64,
    pub actions: Vec<PaceAction>,
    pub reason: String,
}
```

- [ ] **Step 2: Write the failing test**

Append to `core/src/pacer.rs` tests module. This builds a minimal snapshot with a
`GovernorStatus` whose binding tank is over-target, one foreground session and two
background sessions, and asserts the plan pauses background (never foreground):

```rust
    use crate::model::{
        GovernorStatus, PaceAction, Session, Snapshot, Tank, BudgetSource,
    };

    fn tank_over_target(now: i64) -> Tank {
        // 100M budget, 90M used, resets in 100 min → 10M remaining / 100 = 100k/min
        // target. Rate 500k/min → well over target, price should rise.
        Tank {
            used: 90_000_000,
            budget: Some(100_000_000),
            budget_source: BudgetSource::Reported,
            window_start: now - 60 * 60_000,
            resets_at: Some(now + 100 * 60_000),
            rate_per_min: 500_000.0,
            cruise_per_min: Some(100_000.0),
            delta: Some(5.0),
            range_min: Some(20.0),
            wall_at: Some(now + 20 * 60_000),
        }
    }

    fn sess(name: &str, pid: i32, entry: &str, last_user_ms: i64, tpm: f64) -> Session {
        let mut s = Session::default_for_test(); // helper added below
        s.name = name.into();
        s.pid = Some(pid);
        s.entrypoint = entry.into();
        s.last_activity = Some(last_user_ms);
        s.tokens_per_min = tpm;
        s
    }

    fn fleet_sess(name: &str, pid: i32, wf_name: &str, n: usize, now: i64, tpm: f64) -> Session {
        let mut s = sess(name, pid, "workflow", now - 5_000, tpm);
        // One `Workflow` node with `n` child agents. If `Agent`'s fields differ,
        // the serde error names the mismatch — adjust the JSON to match.
        let child = serde_json::json!({
            "id":"wf/a","subagent_type":"workflow-subagent","description":"",
            "model":null,"state":"running","started_at":null,
            "tokens":{"input":0,"output":0,"cache_write":0,"cache_read":0,
                      "web_search":0,"web_fetch":0,"messages":0},
            "tokens_per_min":0.0,"activity":[],"last_activity":null,"children":[]
        });
        let node = serde_json::json!({
            "id":"wf_x","subagent_type":"Workflow","description":wf_name,"model":null,
            "state":"running","started_at":null,
            "tokens":{"input":0,"output":0,"cache_write":0,"cache_read":0,
                      "web_search":0,"web_fetch":0,"messages":0},
            "tokens_per_min":0.0,"activity":[],"last_activity":null,
            "children": vec![child; n]
        });
        s.agents = vec![serde_json::from_value(node).expect("valid test agent")];
        s
    }

    #[test]
    fn plan_paces_background_fleet_first_and_never_foreground() {
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        snap.sessions = vec![
            sess("foreground", 10, "claude-vscode", now - 5_000, 200_000.0),
            sess("loopA", 20, "loop", now - 5_000, 300_000.0),          // biggest burner
            fleet_sess("workflowB", 30, "score_v3", 52, now, 100_000.0), // smaller, but a fleet
        ];
        let (planr, state) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);

        assert!(state.price > 0.0, "price should rise when over target");
        assert!(planr.target_rate > 0.0);
        assert!(!planr.actions.is_empty(), "expected pacing actions");

        // Foreground (pid 10) is never paused.
        for a in &planr.actions {
            if let PaceAction::Pause { pid, .. } = a {
                assert_ne!(*pid, 10, "must never pause the foreground session");
            }
        }
        // The fleet is paused FIRST (before the bigger non-fleet burner) and its
        // action is labeled as the fleet with its agent count.
        match &planr.actions[0] {
            PaceAction::Pause { pid, reason } => {
                assert_eq!(*pid, 30, "fleet-session paused first, even though it burns less");
                assert!(reason.contains("fleet score_v3 (52 agents)"), "reason: {reason}");
            }
            _ => panic!("first action should be a Pause"),
        }
    }
```

- [ ] **Step 3: Add the test-only `Session` helper**

`Session` has many fields; give tests a zeroed constructor. In `core/src/model.rs`, inside `impl Session` (create the block if absent), add:

```rust
    #[cfg(test)]
    pub fn default_for_test() -> Session {
        // All zero/empty; tests set only the fields they assert on.
        serde_json::from_str(
            r#"{"id":"","name":"","title":null,"cwd":"","pid":null,"kind":"interactive",
                "entrypoint":"","version":"","model":null,"host":{"kind":"local"},
                "state":"idle","started_at":null,"last_activity":null,
                "tokens":{"input":0,"output":0,"cache_write":0,"cache_read":0,
                          "web_search":0,"web_fetch":0,"messages":0},
                "tokens_per_min":0.0,"cpu_pct":0.0,"rss_mb":0,"agents":[],"tasks":[],
                "watchers":[],"activity":[],"processes":[]}"#,
        )
        .expect("valid test session")
    }
```

Note: if this fails to deserialize, run `cargo test` and adjust the JSON to match `Session`'s actual fields (the compiler/serde error names the missing/extra field). Keep it in a `#[cfg(test)]` block so it never ships.

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p ccwatch-core pacer::tests::plan_paces_background_fleet_first_and_never_foreground`
Expected: FAIL — `PacerState` / `PacerConfig` / `plan` not found.

- [ ] **Step 5: Implement `plan`**

Add to `core/src/pacer.rs`:

```rust
use crate::model::{Agent, PaceAction, PacingPlan, Session, Snapshot};

/// A Background session considered for pausing.
struct Candidate {
    pid: i32,
    label: String,
    burn: f64,
    is_fleet: bool,
}

/// `(true, "fleet <name> (N agents)")` if the session's live work is a Workflow
/// (a `Workflow` node with children); else `(false, "session <name>")`. Lets the
/// planner prefer fleet-sessions and label every action the way the user thinks.
fn fleet_label(s: &Session) -> (bool, String) {
    for a in &s.agents {
        if a.subagent_type == "Workflow" && !a.children.is_empty() {
            let n = count_agents(&a.children);
            let name = if a.description.is_empty() { "workflow" } else { a.description.as_str() };
            return (true, format!("fleet {name} ({n} agents)"));
        }
    }
    (false, format!("session {}", s.name))
}

fn count_agents(ags: &[Agent]) -> usize {
    ags.iter().map(|a| 1 + count_agents(&a.children)).sum()
}

/// Threaded controller state: just the scalar pace price.
#[derive(Clone, Copy, Debug)]
pub struct PacerState {
    pub price: f64,
}

/// Runtime knobs for the pacer (parsed from `[cruise]` config in Task 6).
#[derive(Clone, Copy, Debug)]
pub struct PacerConfig {
    pub reserve: u64,
    pub deadline_ms: Option<i64>,
    pub eta: f64,
    pub aimd_cut: f64,
    pub idle_secs: i64,
    pub dead_band: f64,
}

impl Default for PacerConfig {
    fn default() -> Self {
        PacerConfig {
            reserve: 0,
            deadline_ms: None,
            eta: 1e-9,
            aimd_cut: 4.0,
            idle_secs: 120,
            dead_band: 0.1,
        }
    }
}

/// Compute the pacing plan for one snapshot. Pure: `now_ms` and `saw_429` are
/// inputs, `prev` carries the price forward. Emits pause actions on Background
/// sessions whose burn exceeds their price-set share, ordered by the biggest
/// overage first, until projected burn ≤ target. Foreground is never touched.
pub fn plan(
    snap: &Snapshot,
    cfg: &PacerConfig,
    prev: PacerState,
    now_ms: i64,
    saw_429: bool,
) -> (PacingPlan, PacerState) {
    let Some(g) = &snap.governor else {
        return (
            PacingPlan {
                target_rate: 0.0,
                actual_rate: 0.0,
                price: prev.price,
                actions: vec![],
                reason: "no governor data".into(),
            },
            prev,
        );
    };
    let (tank, _is_week) = g.binding();
    let remaining = tank
        .budget
        .map(|b| b.saturating_sub(tank.used))
        .unwrap_or(0);
    let deadline = cfg.deadline_ms.or(tank.resets_at);
    let mins = deadline.map(|d| (d - now_ms) as f64 / 60_000.0).unwrap_or(0.0);
    let target = target_rate(remaining, cfg.reserve, mins);
    let actual = tank.rate_per_min;

    // Update the price: AIMD brake on a fresh 429, else dual gradient ascent.
    let price = if saw_429 {
        aimd_on_429(prev.price, cfg.aimd_cut)
    } else {
        update_price(prev.price, actual, target, cfg.eta)
    };

    // Candidate throttle units = Background sessions with a live pid. Each knows
    // whether it's a fleet (its live work is a Workflow) and carries a human label
    // so fleet-sessions pause first and every action reads as the fleet.
    let mut candidates: Vec<Candidate> = snap
        .sessions
        .iter()
        .filter(|s| {
            priority_of(&s.entrypoint, s.last_activity, now_ms, cfg.idle_secs)
                == Priority::Background
        })
        .filter_map(|s| {
            let pid = s.pid?;
            let (is_fleet, label) = fleet_label(s);
            Some(Candidate { pid, label, burn: s.tokens_per_min, is_fleet })
        })
        .collect();

    // Pause fleet-sessions first, then the biggest burners, until projected burn
    // ≤ target — but only when over target by more than the dead-band (anti-flap).
    let mut actions = Vec::new();
    if target > 0.0 && actual > target * (1.0 + cfg.dead_band) {
        candidates.sort_by(|a, b| {
            b.is_fleet
                .cmp(&a.is_fleet)
                .then(b.burn.partial_cmp(&a.burn).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut projected = actual;
        let share = allowed_burn(1.0, price); // per-unit ceiling
        for c in candidates {
            if projected <= target {
                break;
            }
            if c.burn > share {
                actions.push(PaceAction::Pause {
                    pid: c.pid,
                    reason: format!("pause {}: {:.0}/min over pace share", c.label, c.burn),
                });
                projected -= c.burn;
            }
        }
    }

    let reason = if actions.is_empty() {
        format!("coasting: {actual:.0} ≤ target {target:.0}/min")
    } else {
        format!("{} over target → pausing {} background session(s)", actual as u64, actions.len())
    };
    (
        PacingPlan { target_rate: target, actual_rate: actual, price, actions, reason },
        PacerState { price },
    )
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p ccwatch-core pacer::`
Expected: all pass. If `default_for_test` JSON errors, fix per Step 3's note and re-run.

- [ ] **Step 7: Clippy clean**

Run: `cargo clippy -p ccwatch-core --all-targets -- -D warnings`
Expected: no errors.

- [ ] **Step 8: Commit**

```bash
git add core/src/model.rs core/src/pacer.rs
git commit -m "feat(pacer): plan() — dual-price pacing over background sessions"
```

---

### Task 6: `[cruise]` config parsing

Parse the pacer knobs from `~/.claude/ccwatch/config.toml` into `PacerConfig`,
plus a `mode` string. Follow the existing hand-rolled parser in `config.rs`.

**Files:**
- Modify: `core/src/config.rs`

**Interfaces:**
- Consumes: the existing `Config` struct and its line parser (`set_i64`, inline-`#`-comment stripping).
- Produces: `Config` gains `pub cruise_mode: String` (default `"off"`), `pub cruise_reserve: u64` (default 0), `pub cruise_eta: f64` (default `1e-9`), `pub cruise_aimd_cut: f64` (default `4.0`), `pub cruise_dead_band: f64` (default `0.1`); and `pub fn pacer_config(&self) -> ccwatch_core::pacer::PacerConfig` (in-crate: `crate::pacer::PacerConfig`).

- [ ] **Step 1: Write the failing test**

Find the config test module: `grep -n "mod tests" core/src/config.rs`. Append a test that parses a config string (mirror how existing config tests build a `Config` — if they parse from a file path, write to a temp file; if there is a `parse_str`, use it):

```rust
    #[test]
    fn parses_cruise_block() {
        let toml = "\
cruise_mode = \"advisory\"  # off|advisory|oneclick|auto
cruise_reserve = 20_000_000
cruise_eta = 0.000001
";
        let c = Config::parse_for_test(toml); // helper mirrored from existing tests
        assert_eq!(c.cruise_mode, "advisory");
        assert_eq!(c.cruise_reserve, 20_000_000);
        assert!((c.cruise_eta - 1e-6).abs() < 1e-12);
        // Defaults for unset keys.
        assert_eq!(c.cruise_aimd_cut, 4.0);
    }
```

If `Config` has no `parse_for_test`/`parse_str`, check how existing tests construct a `Config` from text (`grep -n "fn parse\|from_str\|Config::load\|parse_line" core/src/config.rs`) and use that exact mechanism instead; adjust the helper name in the test to match.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ccwatch-core config::tests::parses_cruise_block`
Expected: FAIL — fields not found.

- [ ] **Step 3: Implement**

In `core/src/config.rs`: add the fields to `struct Config` with their defaults in the `Default` impl (match the existing style), add match arms in the line parser next to the existing ones:

```rust
"cruise_mode" => cfg.cruise_mode = v.trim().trim_matches('"').to_string(),
"cruise_reserve" => set_u64(&mut cfg.cruise_reserve, v),
"cruise_eta" => set_f64(&mut cfg.cruise_eta, v),
"cruise_aimd_cut" => set_f64(&mut cfg.cruise_aimd_cut, v),
"cruise_dead_band" => set_f64(&mut cfg.cruise_dead_band, v),
```

If `set_u64` / `set_f64` helpers don't exist next to `set_i64`, add them mirroring `set_i64` (parse after stripping `_` separators and inline `#` comments, matching the existing `set_i64` body). Then add the constructor:

```rust
impl Config {
    /// Build the pacer's runtime config from these settings.
    pub fn pacer_config(&self) -> crate::pacer::PacerConfig {
        crate::pacer::PacerConfig {
            reserve: self.cruise_reserve,
            deadline_ms: None,
            eta: self.cruise_eta,
            aimd_cut: self.cruise_aimd_cut,
            idle_secs: self.idle_secs,
            dead_band: self.cruise_dead_band,
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ccwatch-core config::`
Expected: the new test and all existing config tests pass.

- [ ] **Step 5: Commit**

```bash
git add core/src/config.rs
git commit -m "feat(config): parse [cruise] pacer settings"
```

---

### Task 7: Attach the plan to the snapshot (compute-only)

Make the plan observable: `Snapshot` gains `pacing: Option<PacingPlan>`, the
daemon computes it each refresh (threading `PacerState` across refreshes) and
attaches it. Nothing is executed — this is the Advisory data source.

**Files:**
- Modify: `core/src/model.rs` (`Snapshot.pacing` field + `empty()`)
- Modify: `daemon/src/main.rs` (compute + attach in the refresher)

**Interfaces:**
- Consumes: `pacer::plan(...)`, `Config::pacer_config()`, the existing refresher closure that already computes `snap.governor`.
- Produces: `Snapshot.pacing: Option<PacingPlan>` in the JSON snapshot.

- [ ] **Step 1: Add the field**

In `core/src/model.rs`, in `struct Snapshot`:

```rust
    /// Cruise Control's advisory plan for this snapshot (compute-only in Step 1).
    #[serde(default)]
    pub pacing: Option<PacingPlan>,
```

And in `Snapshot::empty(...)`, add `pacing: None,` to the constructor. Also add `pacing: None,` to any other `Snapshot { .. }` literal the compiler flags (e.g. in `remote.rs`, `engine.rs`, `tui/src/app.rs` test builders) — run `cargo build -p ccwatch-core` and fix each error site.

- [ ] **Step 2: Write the failing test (daemon threads state + attaches plan)**

In `daemon/src/main.rs`, locate the refresher closure that sets `snap.governor = Some(g)` (grep: `grep -n "snap.governor = Some" daemon/src/main.rs`). We will compute the plan right after. First add a unit test near the bottom of `daemon/src/main.rs` (or reuse an existing test module) that exercises the helper we extract:

```rust
#[cfg(test)]
mod pacing_tests {
    use ccwatch_core::model::Snapshot;
    use ccwatch_core::pacer::{PacerState, PacerConfig};

    #[test]
    fn plan_attached_when_governor_present_is_computed() {
        // A snapshot with no governor yields a None-ish plan but must not panic,
        // and state price is carried through.
        let snap = Snapshot::empty(1_000);
        let (planr, st) = ccwatch_core::pacer::plan(
            &snap, &PacerConfig::default(), PacerState { price: 0.5 }, 1_000, false,
        );
        assert_eq!(st.price, 0.5, "price carried through when no governor");
        assert!(planr.actions.is_empty());
    }
}
```

- [ ] **Step 3: Run test to verify it compiles+passes**

Run: `cargo test -p ccwatch-daemon pacing_tests`
Expected: PASS (this validates the wiring types line up; `plan` with no governor returns carried price).

- [ ] **Step 4: Compute + attach in the refresher**

In `daemon/src/main.rs`, add a `PacerState` that persists across refreshes. Where the refresher owns its long-lived locals (near the engine), add:

```rust
let mut pacer_state = ccwatch_core::pacer::PacerState { price: 0.0 };
```

Then, immediately after `snap.governor = Some(g);` in the build closure, add (adjust `saw_429` to whatever recent-429 signal is in scope — pass `false` if none is readily available at this site):

```rust
let saw_429 = false; // Step 1: no 429 threading yet; Autonomous step wires this.
let (plan, next_state) = ccwatch_core::pacer::plan(
    &snap,
    &config.pacer_config(),
    pacer_state,
    snap.generated_at,
    saw_429,
);
pacer_state = next_state;
snap.pacing = Some(plan);
```

If `pacer_state` cannot be captured mutably in that closure (borrow/move constraints), thread it the same way `learned`/watermark state is already threaded in the refresher — follow the existing pattern in that function.

- [ ] **Step 5: Build and smoke-test with `--once`**

Run:
```bash
cargo build --release --bin ccwatchd
./target/release/ccwatchd --once | python3 -c "import sys,json; d=json.load(sys.stdin); print('pacing:', d.get('pacing'))"
```
Expected: prints a `pacing` object with `target_rate`, `actual_rate`, `price`, `actions`, `reason` (values depend on live data; `actions` likely `[]` while coasting).

- [ ] **Step 6: Full gate**

Run:
```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: clippy clean, all tests pass.

- [ ] **Step 7: Commit**

```bash
git add core/src/model.rs daemon/src/main.rs
git commit -m "feat(pacer): compute the pacing plan each refresh and attach to the snapshot"
```

---

## Self-Review

**Spec coverage (Step 1 scope):**
- Setpoint (coast + reservation) → Task 1 + Task 5 (`deadline_ms`/`reserve` wired). ✓
- Priority tiers + foreground exemption + **fleet-first pausing/labeling** → Task 2 + Task 5 (`plan` filters to Background, never emits foreground, pauses fleet-sessions first, labels them "fleet <name> (N agents)"). ✓
- Dual-price controller (NUM/Kelly) + proportional weighting → Task 3 (`update_price`, `allowed_burn`) + Task 5. Weighting is equal-weight for now; per-tier weights are a later step (noted). ✓ (partial, intentional)
- AIMD 429 overlay → Task 4 + Task 5 (`saw_429` path; daemon threads `false` until the Autonomous step). ✓
- Dead-band / anti-flap → Task 5 (`dead_band` gate). Minimum dwell time is deferred to the Autonomous step (only matters once actions execute). ✓ (partial, intentional — no enforcement yet so no flapping yet)
- `pacer.rs` pure, `now_ms` an argument → all tasks. ✓
- Config `[cruise]` → Task 6. ✓
- Snapshot carries the plan → Task 7. ✓
- Enforcement → explicitly OUT of Step 1. ✓

**Not possible (not deferred):** per-agent / fleet-subset pausing. Agents run in-process (one `claude` process per session), so there is no per-agent OS handle; the throttle unit is the session, and a fleet-session is paused whole (all its agents at once), labeled as the fleet.

**Deferred to later plans (by design):** the actuator executing actions (One-click/Autonomous), per-tier priority weights, minimum dwell time, 429 threading into the daemon, and all UI (Advisory chart, buttons, reservation picker, action log).

**Placeholder scan:** none — every step has concrete code or an exact command. The two "adjust to the real field name / real parser mechanism" notes (Task 2 Step 6, Task 6 Step 1) are verification steps with a named fallback, not placeholders.

**Type consistency:** `PacerState { price: f64 }`, `PacerConfig { reserve, deadline_ms, eta, aimd_cut, idle_secs, dead_band }`, `plan(&Snapshot, &PacerConfig, PacerState, i64, bool) -> (PacingPlan, PacerState)`, `PacingPlan { target_rate, actual_rate, price, actions, reason }`, `PaceAction::{Pause{pid,reason}, Resume{pid}}`, `Priority::{High,Normal,Background}` — used consistently across Tasks 2–7.
