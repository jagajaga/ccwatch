//! Cruise Control — the Governor's enforcement policy. Pure and deterministic:
//! a snapshot in, a pacing plan out. A single dual-price controller paces the
//! fleet's burn and shares the budget fairly at once (Kelly NUM / dual mirror
//! descent); an AIMD overlay handles the 429 shock. No enforcement here.

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
}
