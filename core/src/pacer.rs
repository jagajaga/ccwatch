//! Cruise Control — the Governor's enforcement policy. Pure and deterministic:
//! a snapshot in, a pacing plan out. A single dual-price controller paces the
//! fleet's burn and shares the budget fairly at once (Kelly NUM / dual mirror
//! descent); an AIMD overlay handles the 429 shock. No enforcement here.

use std::collections::HashSet;
use crate::model::{Agent, PaceAction, PacingPlan, Priority, Session, Snapshot};

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
    let actual = tank.rate_per_min;

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
    let mut candidates: Vec<Candidate> = snap
        .sessions
        .iter()
        .filter(|s| {
            s.tokens_per_min > 0.0
                && priority_of(&s.entrypoint, s.last_activity, now_ms, cfg.idle_secs)
                    == Priority::Background
        })
        .filter_map(|s| {
            let pid = s.pid?;
            let (is_fleet, label) = fleet_label(s);
            Some(Candidate { pid, label, burn: s.tokens_per_min, is_fleet })
        })
        .collect();

    // Greedy value-density knapsack, tiered: fleets are a strict harm-tier and
    // always shed before non-fleet Background (pausing autonomous, unwatched work
    // is low-harm — a barely-burning fleet still goes before a heavy loop). Within
    // a tier, pause the lowest value-density (`weight/burn`) first. Only act when
    // over target by more than the dead-band (anti-flap). `price` = the
    // value-density at the cut (λ).
    let over = target > 0.0 && actual > target * (1.0 + cfg.dead_band);
    let mut actions = Vec::new();
    let mut price = 0.0;
    if over {
        candidates.sort_by(|a, b| {
            b.is_fleet
                .cmp(&a.is_fleet) // fleets (true) before non-fleets
                .then(density(a).partial_cmp(&density(b)).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut projected = actual;
        for c in &candidates {
            if projected <= target {
                price = density(c); // first session we keep sets the cut price
                break;
            }
            actions.push(PaceAction::Pause {
                pid: c.pid,
                reason: format!("pause {}: {:.0}/min (value-density {:.1e})", c.label, c.burn, density(c)),
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

/// Given the pids the current plan wants paused and the set Cruise has already
/// paused, return `(to_pause, to_resume)`: newly-named pids to pause, and
/// already-paced pids no longer in the plan to resume (recovery). Pure.
pub fn reconcile_paced(plan_pause: &[i32], paced: &HashSet<i32>) -> (Vec<i32>, Vec<i32>) {
    let want: HashSet<i32> = plan_pause.iter().copied().collect();
    let to_pause: Vec<i32> = plan_pause.iter().copied().filter(|p| !paced.contains(p)).collect();
    let to_resume: Vec<i32> = paced.iter().copied().filter(|p| !want.contains(p)).collect();
    (to_pause, to_resume)
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
            sess("loopBig", 50, "loop", now - 5_000, 50_000.0), // heavier non-fleet
        ];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);
        assert!(
            matches!(&planr.actions[0], PaceAction::Pause { pid, .. } if *pid == 40),
            "the low-burn fleet must pause before the bigger non-fleet loop"
        );
    }

    #[test]
    fn price_is_finite_and_snapshot_round_trips_with_an_idle_candidate() {
        // An idle (0 tok/min) Background session must not make price INFINITY
        // (which serializes to null and drops the whole snapshot on the client).
        let now = 1_000_000_000_000;
        let mut snap = Snapshot::empty(now);
        snap.governor = Some(GovernorStatus { window: tank_over_target(now), week: None });
        snap.sessions = vec![
            sess("burner", 20, "loop", now - 5_000, 300_000.0),
            sess("idle", 21, "loop", now - 5_000, 0.0), // idle background, pid present
        ];
        let (planr, _) = plan(&snap, &PacerConfig::default(), PacerState { price: 0.0 }, now, false);
        assert!(planr.price.is_finite(), "price must stay finite, got {}", planr.price);
        snap.pacing = Some(planr);
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("\"price\":null"), "price must not serialize to null");
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert!(back.pacing.is_some(), "snapshot with pacing must round-trip");
    }

    #[test]
    fn reconcile_pauses_new_and_resumes_recovered() {
        use std::collections::HashSet;
        // Currently pacing pids {2,3}. The plan now wants {3,4}.
        // → pause 4 (new), resume 2 (no longer in the plan), leave 3.
        let paced: HashSet<i32> = [2, 3].into_iter().collect();
        let (to_pause, to_resume) = reconcile_paced(&[3, 4], &paced);
        assert_eq!(to_pause, vec![4]);
        assert_eq!(to_resume, vec![2]);
    }

    #[test]
    fn reconcile_empty_plan_resumes_all_paced() {
        use std::collections::HashSet;
        let paced: HashSet<i32> = [7, 8].into_iter().collect();
        let (to_pause, mut to_resume) = reconcile_paced(&[], &paced);
        to_resume.sort();
        assert!(to_pause.is_empty());
        assert_eq!(to_resume, vec![7, 8]);
    }
}
