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
}
