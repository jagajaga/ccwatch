//! Cruise Control — the Governor's enforcement policy. Pure and deterministic:
//! a snapshot in, a pacing plan out. A single dual-price controller paces the
//! fleet's burn and shares the budget fairly at once (Kelly NUM / dual mirror
//! descent); an AIMD overlay handles the 429 shock. No enforcement here.

use std::collections::HashSet;
use crate::model::{Agent, Host, PaceAction, PaceTarget, PacingPlan, Priority, Session, Snapshot};

/// The automatic tier, judged purely by a real **user turn**: a session you
/// actually typed in within `idle_secs` is the foreground → `High` (protected);
/// everything else defaults to `Normal` (Cruise's to decide). Note this keys off
/// the *user* turn, not `last_activity` — a session grinding autonomously bumps
/// its activity every second but has no recent user turn, so it is correctly
/// `Normal`, not self-protecting.
pub fn auto_priority(last_user_turn: Option<i64>, now_ms: i64, idle_secs: i64) -> Priority {
    let user_recent = last_user_turn.is_some_and(|t| now_ms - t <= idle_secs * 1000);
    if user_recent {
        Priority::High
    } else {
        Priority::Normal
    }
}

/// A session's **effective** tier: the user's per-session override if they set one
/// (`High` = never pause, `Low` = shed first), else the auto tier. The override
/// always wins — pinning `Low` on the session you're typing in makes it pausable;
/// pinning `High` on a background one protects it.
pub fn effective_priority(s: &Session, now_ms: i64, idle_secs: i64) -> Priority {
    s.priority_override
        .unwrap_or_else(|| auto_priority(s.last_user_turn, now_ms, idle_secs))
}

/// Billable tok/min that spends `remaining - reserve` evenly until the deadline.
/// `reserve == 0` and `deadline == reset` gives the Governor's coast pace.
pub fn target_rate(remaining: u64, reserve: u64, mins_to_deadline: f64) -> f64 {
    if mins_to_deadline <= 0.0 {
        return 0.0;
    }
    let spendable = remaining.saturating_sub(reserve) as f64;
    spendable / mins_to_deadline
}

/// Smallest price we allow. A multiplicative update can't climb back from an
/// exact zero, so λ is floored here; `1 / MIN_PRICE` is the effective "unbounded"
/// burn when the budget isn't binding.
const MIN_PRICE: f64 = 1e-9;
/// Ceiling on λ. At this price `allowed_burn = weight / 1e9 ≈ 0` (throttle
/// everything), but it is *finite and recoverable*: near budget exhaustion the
/// relative error explodes and `exp()` can overflow, so without a ceiling λ would
/// latch at `+∞` and deadlock a unit at zero burn even after the budget recovers.
/// `inf.clamp(_, MAX_PRICE) == MAX_PRICE`, so the overflow is caught here.
const MAX_PRICE: f64 = 1e9;

/// Smallest non-zero price the brake floors to, so an AIMD cut from price 0 still
/// throttles hard instead of multiplying zero.
const AIMD_FLOOR: f64 = 1e-6;

/// Dual **mirror descent** on the budget constraint (Balseiro et al.): the pace
/// price `λ` moves *multiplicatively* by the **relative** pace error, so one
/// dimensionless step size `eta` (~0.05) is stable across tank scales — a plain
/// additive `λ + η·(burn − target)` would need `η < 2/target²`, i.e. a different
/// η per tank. Over target → λ grows → allowed burn shrinks, and vice-versa. λ is
/// kept in `[MIN_PRICE, MAX_PRICE]` so it stays positive and can never overflow.
pub fn update_price(prev: f64, actual_burn: f64, target_rate: f64, eta: f64) -> f64 {
    let base = prev.clamp(MIN_PRICE, MAX_PRICE);
    if target_rate <= 0.0 {
        // No spendable budget → drive the price up hard (throttle everything).
        return (base * eta.exp()).clamp(MIN_PRICE, MAX_PRICE);
    }
    let rel_err = (actual_burn - target_rate) / target_rate;
    (base * (eta * rel_err).exp()).clamp(MIN_PRICE, MAX_PRICE)
}

/// A unit's allowed burn under the current price: `weight / λ`. Higher price
/// throttles everyone; higher weight (priority) buys a bigger share. A
/// non-positive price means the budget isn't binding → unbounded.
pub fn allowed_burn(weight: f64, price: f64) -> f64 {
    if price <= 0.0 {
        f64::INFINITY
    } else {
        weight / price
    }
}

/// AIMD multiplicative-increase on a fresh 429: jump the price by `cut`×. From a
/// zero price, floor first so the brake actually bites; cap at `MAX_PRICE` so it
/// obeys the same bound as `update_price` (its result becomes the new `λ`).
pub fn aimd_on_429(price: f64, cut: f64) -> f64 {
    (price.max(AIMD_FLOOR) * cut).min(MAX_PRICE)
}

/// A Background session considered for pausing. `ssh` is `None` for a local
/// session (paused with a local `kill(pid, SIGSTOP)`) and `Some(target)` for a
/// remote one (paused with `ssh <target> kill -STOP <pid>`). The actuator is
/// chosen by this field, so a remote pid is never signalled as a local pid.
struct Candidate {
    pid: i32,
    ssh: Option<String>,
    label: String,
    burn: f64,
    is_fleet: bool,
    /// User-pinned `Low` — a strict tier shed before any `Normal` session.
    is_low: bool,
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

/// Keep-value of a Background session. Fleets — autonomous, unwatched, high-burn —
/// are worth the least, so they shed first; other Background work is worth more.
fn weight_of(is_fleet: bool) -> f64 {
    if is_fleet {
        1.0
    } else {
        4.0
    }
}

/// Value density = keep-value per token/min. Pausing the lowest-density sessions
/// first sheds the least-valuable work per token — the greedy Lagrangian-knapsack
/// solution to "keep the most value within the target". The density at the cut IS
/// the pace price λ.
fn density(c: &Candidate) -> f64 {
    if c.burn <= 0.0 {
        f64::INFINITY
    } else {
        weight_of(c.is_fleet) / c.burn
    }
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
            eta: 0.05, // dimensionless mirror-descent step (see Task 3)
            aimd_cut: 4.0,
            idle_secs: 120,
            dead_band: 0.1,
        }
    }
}

/// Compute the pacing plan for one snapshot. Pure: `now_ms` and `saw_429` are
/// inputs. Selection is a greedy **value-density knapsack** for our discrete
/// whole-session actuator: fleets are a strict harm-tier that always sheds before
/// non-fleet Background, and within a tier the lowest value-density (`weight /
/// burn`) pauses first, until projected burn ≤ target. The reported price `λ` is
/// the value-density at the cut. Foreground is never touched. (The mirror-descent
/// primitives `update_price` / `aimd_on_429` provide the smooth continuous price
/// for a future finer-grained actuator; the discrete knapsack here is exact and
/// immediate.)
///
/// Note the price semantics: `price` (and `PacerState.price`) is the value-density
/// at the knapsack cut (`weight/burn`), NOT the mirror-descent λ from
/// `update_price`. A future continuous controller must not seed `update_price` with
/// it — the two are different quantities that happen to share the `λ` name.
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
                auto: false,
                paced: 0,
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
    let mut target = target_rate(remaining, cfg.reserve, mins);
    // Sanitize the burn: a non-finite `actual_rate` serializes to JSON `null`,
    // and the client's non-optional `f64` then drops the whole snapshot. Same
    // hazard `price` is guarded against below.
    let actual = if tank.rate_per_min.is_finite() {
        tank.rate_per_min
    } else {
        0.0
    };

    // A fresh 429 is a hard signal the smooth pace is too slow: pace to a fraction
    // of target until it clears (AIMD, expressed in target space).
    if saw_429 {
        target /= cfg.aimd_cut.max(1.0);
    }

    // Candidate throttle units = Background sessions that are actually burning and
    // have a live pid (foreground / High is excluded before we ever plan). Each
    // knows whether it's a fleet and carries a human label so every action reads
    // the way the user thinks. Zero-burn sessions are excluded: pausing one does
    // nothing, and its infinite value-density would poison the cut price.
    //
    // Actuator by host: a Local session is paused with a local `kill(pid,SIGSTOP)`
    // (`ssh: None`); a Remote session is paused with `ssh <target> kill -STOP`
    // (`ssh: Some(target)`), so a remote pid is NEVER signalled as a local pid.
    // Cloud sessions have no reachable actuator and are excluded.
    let mut candidates: Vec<Candidate> = snap
        .sessions
        .iter()
        .filter(|s| {
            s.tokens_per_min > 0.0
                && !matches!(s.host, Host::Cloud)
                && effective_priority(s, now_ms, cfg.idle_secs) != Priority::High
        })
        .filter_map(|s| {
            let pid = s.pid?;
            let ssh = match &s.host {
                Host::Remote { ssh_target, .. } => Some(ssh_target.clone()),
                _ => None,
            };
            let is_low = effective_priority(s, now_ms, cfg.idle_secs) == Priority::Low;
            let (is_fleet, label) = fleet_label(s);
            Some(Candidate { pid, ssh, label, burn: s.tokens_per_min, is_fleet, is_low })
        })
        .collect();

    // Greedy value-density knapsack, tiered. Strict harm-tiers shed in order:
    // (1) user-pinned `Low` first, then (2) `Normal` fleets (autonomous, unwatched
    // work — a barely-burning fleet still goes before a heavy loop), then (3)
    // `Normal` non-fleets. Within a tier, pause the lowest value-density
    // (`weight/burn`) first. Only act when over target by more than the dead-band
    // (anti-flap). `price` = the value-density at the cut (λ).
    let over = target > 0.0 && actual > target * (1.0 + cfg.dead_band);
    let mut actions = Vec::new();
    let mut price = 0.0;
    if over {
        candidates.sort_by(|a, b| {
            b.is_low
                .cmp(&a.is_low) // Low (true) shed before Normal
                .then(b.is_fleet.cmp(&a.is_fleet)) // then fleets before non-fleets
                .then(density(a).partial_cmp(&density(b)).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut projected = actual;
        for c in &candidates {
            if projected <= target {
                price = density(c); // first session we keep sets the cut price
                break;
            }
            let where_ = match &c.ssh {
                Some(t) => format!(" on {t}"),
                None => String::new(),
            };
            actions.push(PaceAction::Pause {
                pid: c.pid,
                ssh: c.ssh.clone(),
                reason: format!(
                    "pause {}{}: {:.0}/min (value-density {:.1e})",
                    c.label, where_, c.burn, density(c)
                ),
            });
            projected -= c.burn;
            price = density(c); // last session we pause sets the cut price
        }
    }

    // Distinguish "coasting" from "over target but nothing we may pause" — the
    // latter is a real state (only the exempt foreground is left) and must not be
    // reported as coasting.
    let reason = if !over {
        format!("coasting: {actual:.0} ≤ target {target:.0}/min")
    } else if actions.is_empty() {
        format!("over target ({actual:.0} > {target:.0}/min) — no background sessions to pause")
    } else {
        format!("{} over target → pausing {} background session(s)", actual as u64, actions.len())
    };
    // Never report a non-finite price: it serializes to JSON `null` and the client
    // (non-optional f64) drops the whole snapshot. Zero-burn candidates are already
    // excluded, so this is defensive.
    let price = price.min(MAX_PRICE);
    (
        PacingPlan { target_rate: target, actual_rate: actual, price, actions, reason, auto: false, paced: 0 },
        PacerState { price },
    )
}

/// Given the targets the current plan wants paused and the set Cruise has already
/// paused, return `(to_pause, to_resume)`: newly-named targets to pause, and
/// already-paced targets no longer in the plan to resume (recovery). Pure. Keyed
/// by `PaceTarget` (pid + host), so a local and a remote pid that share a number
/// are distinct.
pub fn reconcile_paced(
    plan_pause: &[PaceTarget],
    paced: &HashSet<PaceTarget>,
) -> (Vec<PaceTarget>, Vec<PaceTarget>) {
    let want: HashSet<PaceTarget> = plan_pause.iter().cloned().collect();
    let to_pause: Vec<PaceTarget> =
        plan_pause.iter().filter(|p| !paced.contains(p)).cloned().collect();
    let to_resume: Vec<PaceTarget> =
        paced.iter().filter(|p| !want.contains(p)).cloned().collect();
    (to_pause, to_resume)
}

/// Decide the pause/resume actions the Cruise runtime should take this tick.
///
/// In **auto** mode, reconcile the live paced set toward the plan (pause newly
/// named, resume recovered). In **any non-auto** mode (off/advisory/oneclick),
/// take NO pause or resume action: a manual one-click Apply must persist until an
/// explicit Release (`SetCruiseMode`), so the per-tick loop must not silently
/// resume it. (Pruning targets whose process has exited is the daemon's job — it
/// needs live syscalls this pure function can't make.) Pure.
pub fn cruise_plan_actions(
    auto: bool,
    plan_pause: &[PaceTarget],
    paced: &HashSet<PaceTarget>,
) -> (Vec<PaceTarget>, Vec<PaceTarget>) {
    if auto {
        reconcile_paced(plan_pause, paced)
    } else {
        (Vec::new(), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Priority;

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

    #[test]
    fn recent_user_turn_is_high_else_normal() {
        let now = 1_000_000_000;
        // Typed 10s ago → foreground → High/exempt.
        assert_eq!(auto_priority(Some(now - 10_000), now, 120), Priority::High);
        // No user turn for 10 min → the "started it and walked away" case → Normal
        // (pausable), even though the session may be burning hard right now.
        assert_eq!(auto_priority(Some(now - 600_000), now, 120), Priority::Normal);
        // Never a user turn (headless / workflow) → Normal.
        assert_eq!(auto_priority(None, now, 120), Priority::Normal);
    }

    #[test]
    fn override_wins_over_auto_tier() {
        let now = 1_000_000_000;
        // Pinning Low on the session you're actively typing in makes it pausable.
        let mut active = Session::default_for_test();
        active.last_user_turn = Some(now - 5_000); // auto would be High
        active.priority_override = Some(Priority::Low);
        assert_eq!(effective_priority(&active, now, 120), Priority::Low);
        // Pinning High on an idle/background session protects it.
        let mut background = Session::default_for_test();
        background.last_user_turn = None; // auto would be Normal
        background.priority_override = Some(Priority::High);
        assert_eq!(effective_priority(&background, now, 120), Priority::High);
        // No override → falls back to the auto tier.
        let mut plain = Session::default_for_test();
        plain.last_user_turn = None;
        assert_eq!(effective_priority(&plain, now, 120), Priority::Normal);
    }

    #[test]
    fn price_rises_over_target_and_falls_under() {
        // Multiplicative update on the relative error, dimensionless eta.
        let up = update_price(1.0, 800_000.0, 600_000.0, 0.05);
        assert!(up > 1.0, "over target should raise price, got {up}");
        let down = update_price(1.0, 400_000.0, 600_000.0, 0.05);
        assert!(down < 1.0, "under target should lower price, got {down}");
    }

    #[test]
    fn price_stays_positive_and_finite() {
        // Massively under target must not collapse to zero (a multiplicative
        // update can't recover from 0) nor go negative.
        let p = update_price(0.0, 0.0, 600_000.0, 0.05);
        assert!(p > 0.0 && p.is_finite(), "price stays positive+finite, got {p}");
    }

    #[test]
    fn allowed_burn_is_proportional_to_weight() {
        // Double the weight → double the allowed burn at the same price.
        assert_eq!(allowed_burn(2.0, 0.5), 2.0 * allowed_burn(1.0, 0.5));
    }

    #[test]
    fn loop_converges_at_any_scale() {
        // One unit obeys its price-share each tick; the price drives burn to the
        // target. The SAME dimensionless eta works whether the target is small or
        // large — the whole point of the multiplicative (mirror-descent) form.
        for &target in &[600_000.0_f64, 6_000_000.0] {
            let mut price = 1e-8;
            let mut burn = allowed_burn(1.0, price);
            for _ in 0..3000 {
                price = update_price(price, burn, target, 0.05);
                burn = allowed_burn(1.0, price);
            }
            assert!((burn - target).abs() / target < 0.05, "target {target}: burn {burn}");
        }
    }

    #[test]
    fn price_is_bounded_and_recovers_near_exhaustion() {
        // Near budget exhaustion target_rate → ~0 while burn is normal: the
        // relative error explodes and exp() would overflow. λ must stay finite
        // (≤ MAX_PRICE), not latch at +∞.
        let stuck = update_price(1.0, 600_000.0, 0.001, 0.05);
        assert!(stuck.is_finite() && stuck <= 1e9, "price bounded, got {stuck}");
        // And once healthy under-target readings arrive, λ comes back down off the
        // ceiling (allowed burn rises again) — the deadlock is recoverable.
        let mut p = 1e9;
        for _ in 0..500 {
            p = update_price(p, 0.0, 600_000.0, 0.05);
        }
        assert!(p < 1.0, "price recovers off the ceiling, got {p}");
    }

    #[test]
    fn aimd_multiplicatively_jumps_the_price() {
        // A 429 with cut factor 4 quadruples the price (a hard brake). From a
        // zero/near-zero price it still produces a meaningfully positive price.
        assert_eq!(aimd_on_429(2.0, 4.0), 8.0);
        assert!(aimd_on_429(0.0, 4.0) > 0.0, "must brake even from price 0");
        // Respects the same ceiling as update_price (no runaway past MAX_PRICE).
        assert!(aimd_on_429(1e9, 4.0) <= 1e9, "AIMD stays within MAX_PRICE");
    }

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

    /// `last_user` is the last real USER turn: `Some(recent)` → foreground/High,
    /// `None` → no user turn → Normal (a pausable candidate). The entrypoint no
    /// longer affects the tier; it's kept only for labeling.
    fn sess(name: &str, pid: i32, entry: &str, last_user: Option<i64>, tpm: f64) -> Session {
        let mut s = Session::default_for_test(); // helper added below
        s.name = name.into();
        s.pid = Some(pid);
        s.entrypoint = entry.into();
        s.last_user_turn = last_user;
        s.tokens_per_min = tpm;
        s
    }

    fn fleet_sess(name: &str, pid: i32, wf_name: &str, n: usize, _now: i64, tpm: f64) -> Session {
        // A fleet/workflow session has no human at the keyboard → no user turn.
        let mut s = sess(name, pid, "workflow", None, tpm);
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
            sess("foreground", 10, "claude-vscode", Some(now - 5_000), 200_000.0),
            sess("loopA", 20, "loop", None, 300_000.0),          // biggest burner
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
            PaceAction::Pause { pid, reason, .. } => {
                assert_eq!(*pid, 30, "fleet-session paused first, even though it burns less");
                assert!(reason.contains("fleet score_v3 (52 agents)"), "reason: {reason}");
            }
            _ => panic!("first action should be a Pause"),
        }
    }

    #[test]
    fn a_low_burn_fleet_still_pauses_before_a_bigger_non_fleet() {
        // Fleets are a strict harm-tier: even a barely-burning fleet is shed
        // before a heavier non-fleet loop — value-density alone would (wrongly)
        // pause the loop first, so this guards the tiering.
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        snap.sessions = vec![
            fleet_sess("fleetX", 40, "wf", 3, now, 10_000.0),   // tiny burn, but a fleet
            sess("loopBig", 50, "loop", None, 50_000.0), // heavier non-fleet
        ];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);
        assert!(
            matches!(&planr.actions[0], PaceAction::Pause { pid, .. } if *pid == 40),
            "the low-burn fleet must pause before the bigger non-fleet loop"
        );
    }

    #[test]
    fn remote_sessions_pause_via_ssh_not_a_local_pid() {
        // A Remote session IS pausable — but its pause target must carry the ssh
        // host so the daemon actuates with `ssh <target> kill -STOP`, never a
        // local `kill(pid)` that would freeze whatever local process reuses that
        // pid number. A Local session's target has `ssh: None`.
        use crate::model::Host;
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        let mut remote = sess("remoteLoop", 99, "loop", None, 300_000.0);
        remote.host = Host::Remote { name: "r".into(), ssh_target: "u@h".into() };
        snap.sessions = vec![
            sess("localLoop", 20, "loop", None, 300_000.0), // local background
            remote,                                                // remote background — pause via ssh
        ];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);

        let targets = planr.pause_targets();
        let local = targets.iter().find(|t| t.pid == 20).expect("local pid must be a candidate");
        assert_eq!(local.ssh, None, "a local pause target must have no ssh host");
        let rem = targets.iter().find(|t| t.pid == 99).expect("remote pid must be a candidate");
        assert_eq!(
            rem.ssh.as_deref(),
            Some("u@h"),
            "a remote pause target must carry its ssh host so it's never a local kill"
        );
    }

    #[test]
    fn cloud_sessions_are_never_selected_for_pause() {
        // Cloud sessions have no reachable actuator (no local pid, no ssh target)
        // and must never be selected.
        use crate::model::Host;
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        let mut cloud = sess("cloudLoop", 77, "loop", None, 300_000.0);
        cloud.host = Host::Cloud;
        snap.sessions = vec![
            sess("localLoop", 20, "loop", None, 300_000.0),
            cloud,
        ];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);
        let paused = planr.pause_pids();
        assert!(paused.contains(&20));
        assert!(!paused.contains(&77), "a cloud pid must never be selected for pause");
    }

    #[test]
    fn low_pinned_sheds_before_a_normal_fleet() {
        // A user-pinned Low session is a strict harm-tier: paused before any
        // Normal session — even a heavier Normal fleet, and even though it burns
        // less. Value-density alone would pause the fleet first; the Low tier wins.
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        let mut low = sess("pinnedLow", 60, "cli", None, 20_000.0); // light burn, but Low
        low.priority_override = Some(Priority::Low);
        snap.sessions = vec![
            fleet_sess("normalFleet", 70, "wf", 5, now, 200_000.0), // Normal, heavy fleet
            low,
        ];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);
        assert!(
            matches!(&planr.actions[0], PaceAction::Pause { pid, .. } if *pid == 60),
            "a user-pinned Low session must shed before a Normal fleet"
        );
    }

    #[test]
    fn high_pinned_is_never_paused() {
        // Pinning High protects even a heavy autonomous burner; the Normal session
        // is the one paced instead.
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        let mut high = sess("pinnedHigh", 80, "loop", None, 400_000.0);
        high.priority_override = Some(Priority::High);
        snap.sessions = vec![high, sess("normalLoop", 20, "loop", None, 100_000.0)];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);
        let paused = planr.pause_pids();
        assert!(!paused.contains(&80), "a High-pinned session must never be paused");
        assert!(paused.contains(&20), "the Normal session should be the one paced");
    }

    #[test]
    fn price_is_finite_and_snapshot_round_trips_with_an_idle_candidate() {
        // An idle (0 tok/min) Background session must not make price INFINITY
        // (which serializes to null and drops the whole snapshot on the client).
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        snap.sessions = vec![
            sess("burner", 20, "loop", None, 300_000.0),
            sess("idle", 21, "loop", None, 0.0), // idle background, pid present
        ];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);
        assert!(planr.price.is_finite(), "price must stay finite, got {}", planr.price);
        snap.pacing = Some(planr);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("\"price\":null"), "price must not serialize to null");
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert!(back.pacing.is_some(), "snapshot with pacing must round-trip");
    }

    fn local(pid: i32) -> PaceTarget {
        PaceTarget { pid, ssh: None }
    }

    #[test]
    fn reconcile_pauses_new_and_resumes_recovered() {
        use std::collections::HashSet;
        // Currently pacing pids {2,3}. The plan now wants {3,4}.
        // → pause 4 (new), resume 2 (no longer in the plan), leave 3.
        let paced: HashSet<PaceTarget> = [local(2), local(3)].into_iter().collect();
        let (to_pause, to_resume) = reconcile_paced(&[local(3), local(4)], &paced);
        assert_eq!(to_pause, vec![local(4)]);
        assert_eq!(to_resume, vec![local(2)]);
    }

    #[test]
    fn reconcile_empty_plan_resumes_all_paced() {
        use std::collections::HashSet;
        let paced: HashSet<PaceTarget> = [local(7), local(8)].into_iter().collect();
        let (to_pause, mut to_resume) = reconcile_paced(&[], &paced);
        to_resume.sort_by_key(|t| t.pid);
        assert!(to_pause.is_empty());
        assert_eq!(to_resume, vec![local(7), local(8)]);
    }

    #[test]
    fn reconcile_distinguishes_local_and_remote_same_pid() {
        use std::collections::HashSet;
        // A local pid 5 and a remote pid 5 are DIFFERENT targets: with only the
        // remote paced and the plan wanting only the local, we pause the local and
        // resume the remote — never confuse the two.
        let remote5 = PaceTarget { pid: 5, ssh: Some("u@h".into()) };
        let paced: HashSet<PaceTarget> = [remote5.clone()].into_iter().collect();
        let (to_pause, to_resume) = reconcile_paced(&[local(5)], &paced);
        assert_eq!(to_pause, vec![local(5)]);
        assert_eq!(to_resume, vec![remote5]);
    }

    #[test]
    fn cruise_non_auto_never_resumes_manual_pauses() {
        use std::collections::HashSet;
        // The sticky-Apply invariant: in any non-auto mode the per-tick loop takes
        // NO action, so a manually-applied pause survives until an explicit Release.
        let paced: HashSet<PaceTarget> = [local(7), local(9)].into_iter().collect();
        let (to_pause, to_resume) = cruise_plan_actions(false, &[], &paced);
        assert!(to_pause.is_empty());
        assert!(to_resume.is_empty(), "non-auto must not resume manual pauses");
    }

    #[test]
    fn cruise_auto_reconciles_to_the_plan() {
        use std::collections::HashSet;
        let paced: HashSet<PaceTarget> = [local(7)].into_iter().collect();
        let (to_pause, to_resume) = cruise_plan_actions(true, &[local(9)], &paced);
        assert_eq!(to_pause, vec![local(9)]);
        assert_eq!(to_resume, vec![local(7)]);
    }
}
