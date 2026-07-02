//! The Governor — fuel-gauge math over bucketed usage.
//!
//! Input: `(bucket_ts_ms, billable_tokens)` pairs (5-minute buckets) covering
//! the recent horizon, merged across every host. Output: two [`Tank`]s —
//!
//! - **plan window**: a fixed-length block (default 5h, mirroring Anthropic's
//!   session window) anchored at the first activity after the previous window
//!   expired;
//! - **cruise**: a rolling 1-hour budget the user sets as a self-governor.
//!
//! Pure functions over explicit `now_ms` so every edge is unit-testable.

use crate::config::Config;
use crate::model::{Alert, AlertKind, BudgetSource, GovernorStatus, Severity, Tank};

pub const BUCKET_MS: i64 = 5 * 60 * 1000;
/// Delta ceiling: JSON cannot represent infinity (serde encodes it as null),
/// so "tank empty but still burning" is capped here. UIs render >= this as ⛔.
pub const DELTA_EMPTY: f64 = 99.0;
/// Rate window for the "current speed" readout (matches the engine's default).
const RATE_WINDOW_MS: i64 = 5 * 60 * 1000;

/// Round a timestamp down to its bucket.
pub fn bucket_of(ts_ms: i64) -> i64 {
    ts_ms - ts_ms.rem_euclid(BUCKET_MS)
}

/// Merge bucket lists (e.g. local + remotes) summing same-bucket values.
pub fn merge_buckets(lists: &[&[(i64, u64)]]) -> Vec<(i64, u64)> {
    let mut map = std::collections::BTreeMap::new();
    for list in lists {
        for &(ts, v) in *list {
            *map.entry(bucket_of(ts)).or_insert(0u64) += v;
        }
    }
    map.into_iter().collect()
}

/// Find the current plan-window start: windows are `window_ms` long, the first
/// one starts at the first activity, and after a window expires the next one
/// starts at the first activity that follows. Returns `None` when there is no
/// activity in the current window (tank idle/full).
pub fn window_start(buckets: &[(i64, u64)], now_ms: i64, window_ms: i64) -> Option<i64> {
    let mut start: Option<i64> = None;
    for &(ts, v) in buckets {
        if v == 0 || ts > now_ms {
            continue;
        }
        match start {
            None => start = Some(ts),
            Some(s) if ts >= s + window_ms => {
                // Previous window expired before this activity; a new window
                // anchors here.
                start = Some(ts);
            }
            _ => {}
        }
    }
    // The window containing `now`: if the last anchored window already expired
    // with no later activity, there is no current window.
    match start {
        Some(s) if now_ms < s + window_ms => Some(s),
        _ => None,
    }
}

fn sum_range(buckets: &[(i64, u64)], from_ms: i64, to_ms: i64) -> u64 {
    buckets
        .iter()
        .filter(|(ts, _)| *ts >= bucket_of(from_ms) && *ts <= to_ms)
        .map(|(_, v)| v)
        .sum()
}

fn make_tank(
    used: u64,
    budget: Option<u64>,
    budget_source: BudgetSource,
    window_start: i64,
    resets_at: Option<i64>,
    rate_per_min: f64,
    now_ms: i64,
) -> Tank {
    let remaining = budget.map(|b| b.saturating_sub(used));
    // Cruise only makes sense with a reset deadline and budget.
    let cruise_per_min = match (remaining, resets_at) {
        (Some(rem), Some(reset)) if reset > now_ms => {
            Some(rem as f64 / ((reset - now_ms) as f64 / 60_000.0))
        }
        _ => None,
    };
    let delta = match cruise_per_min {
        Some(c) if c > 0.0 => Some(rate_per_min / c),
        // Budget exhausted but still burning → infinite throttle.
        Some(_) if rate_per_min > 0.0 => Some(DELTA_EMPTY),
        _ => None,
    };
    let range_min = match remaining {
        Some(rem) if rate_per_min > 0.0 => Some(rem as f64 / rate_per_min),
        _ => None,
    };
    let wall_at = match (range_min, resets_at) {
        (Some(r), Some(reset)) => {
            let wall = now_ms + (r * 60_000.0) as i64;
            (wall < reset).then_some(wall)
        }
        _ => None,
    };
    Tank {
        used,
        budget,
        budget_source,
        window_start,
        resets_at,
        rate_per_min,
        cruise_per_min,
        delta,
        range_min,
        wall_at,
    }
}

/// Compute the full governor status. `learned_window_budget` comes from
/// observed 429s and is used when no config budget is set.
pub fn compute(
    buckets: &[(i64, u64)],
    now_ms: i64,
    cfg: &Config,
    learned_window_budget: Option<u64>,
) -> GovernorStatus {
    let window_ms = cfg.governor_window_hours * 3_600_000;
    let rate_per_min =
        sum_range(buckets, now_ms - RATE_WINDOW_MS, now_ms) as f64 / (RATE_WINDOW_MS as f64 / 60_000.0);

    // Plan window tank.
    let (w_budget, w_source) = match (cfg.governor_window_budget, learned_window_budget) {
        (Some(b), _) => (Some(b), BudgetSource::Config),
        (None, Some(b)) => (Some(b), BudgetSource::Learned),
        _ => (None, BudgetSource::Unknown),
    };
    let window = match window_start(buckets, now_ms, window_ms) {
        Some(start) => make_tank(
            sum_range(buckets, start, now_ms),
            w_budget,
            w_source,
            start,
            Some(start + window_ms),
            rate_per_min,
            now_ms,
        ),
        None => make_tank(0, w_budget, w_source, now_ms, None, rate_per_min, now_ms),
    };

    // Rolling cruise tank (last 60 min vs hourly budget). Its "reset" is a
    // fiction — the horizon one hour out — which gives cruise/delta semantics
    // of "at this pace you'd use X% of an hourly budget".
    let hour_ms = 3_600_000;
    let used_hour = sum_range(buckets, now_ms - hour_ms, now_ms);
    let cruise = match cfg.governor_hourly_budget {
        Some(b) => {
            let remaining = b.saturating_sub(used_hour);
            let cruise_per_min = remaining as f64 / 60.0;
            let delta = if cruise_per_min > 0.0 {
                Some(rate_per_min / cruise_per_min)
            } else if rate_per_min > 0.0 {
                Some(DELTA_EMPTY)
            } else {
                None
            };
            Tank {
                used: used_hour,
                budget: Some(b),
                budget_source: BudgetSource::Config,
                window_start: now_ms - hour_ms,
                resets_at: None,
                rate_per_min,
                cruise_per_min: Some(cruise_per_min),
                delta,
                range_min: (rate_per_min > 0.0).then(|| remaining as f64 / rate_per_min),
                wall_at: None,
            }
        }
        None => make_tank(
            used_hour,
            None,
            BudgetSource::Unknown,
            now_ms - hour_ms,
            None,
            rate_per_min,
            now_ms,
        ),
    };

    GovernorStatus { window, cruise }
}

/// The BudgetWall alert when the wall lands before the reset.
pub fn wall_alert(g: &GovernorStatus, now_ms: i64) -> Option<Alert> {
    let wall = g.window.wall_at?;
    // Only alarm when actually burning meaningfully.
    if g.window.rate_per_min < 1.0 {
        return None;
    }
    let mins = (wall - now_ms) / 60_000;
    Some(Alert {
        severity: Severity::Critical,
        kind: AlertKind::BudgetWall,
        subject: "plan window".into(),
        session_id: String::new(),
        message: format!(
            "at {:.0}k/min you hit the limit in ~{}m (reset {}m later)",
            g.window.rate_per_min / 1000.0,
            mins,
            (g.window.resets_at.unwrap_or(wall) - wall) / 60_000
        ),
        since_ms: now_ms,
    })
}

/// Estimate the plan-window budget from observed 429 bursts: for each
/// rate-limit event, the usage accumulated in its window at that moment is a
/// lower bound on the ceiling. The max over events is the best estimate.
pub fn learn_budget(buckets: &[(i64, u64)], rate_limit_ts: &[i64], window_ms: i64) -> Option<u64> {
    let mut best = None;
    for &ts in rate_limit_ts {
        if let Some(start) = window_start(buckets, ts, window_ms) {
            let used = sum_range(buckets, start, ts);
            if used > 0 && best.map(|b| used > b).unwrap_or(true) {
                best = Some(used);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;
    const NOW: i64 = 1_800_000_000_000;

    fn cfg() -> Config {
        Config {
            governor_window_budget: Some(1_000_000),
            governor_hourly_budget: Some(300_000),
            ..Config::default()
        }
    }

    /// Buckets: one entry per (offset-minutes-ago, tokens).
    fn buckets(entries: &[(i64, u64)]) -> Vec<(i64, u64)> {
        merge_buckets(&[&entries
            .iter()
            .map(|(mins_ago, v)| (NOW - mins_ago * 60_000, *v))
            .collect::<Vec<_>>()[..]])
    }

    #[test]
    fn window_anchors_at_first_activity_and_rolls_over() {
        // Activity 6h ago anchors a window covering [T-6h, T-1h); activity
        // 2h ago falls INSIDE it; activity 30m ago (after that window expired)
        // anchors the current window.
        let b = buckets(&[(360, 100), (120, 200), (30, 300)]);
        let start = window_start(&b, NOW, 5 * H).unwrap();
        assert_eq!(start, bucket_of(NOW - 30 * 60_000));

        // No activity in the current window → None.
        let b = buckets(&[(360, 100)]);
        assert!(window_start(&b, NOW, 5 * H).is_none());
    }

    #[test]
    fn used_counts_only_current_window() {
        // Old window: activity 7h ago (window expired 2h ago). Current window
        // anchors at 80m ago; the 30m-ago burn is inside it too.
        let b = buckets(&[(420, 100_000), (80, 200_000), (30, 300_000)]);
        let g = compute(&b, NOW, &cfg(), None);
        assert_eq!(g.window.used, 500_000, "old-window usage must not count");
        assert_eq!(g.window.budget, Some(1_000_000));
        assert_eq!(
            g.window.window_start,
            bucket_of(NOW - 80 * 60_000),
            "window anchors at first activity after the previous one expired"
        );
    }

    #[test]
    fn delta_and_range_math() {
        // Window anchored 2h ago → resets 3h from now. Used 500k of 1M.
        // Recent burn: 50k in the last 5 min → 10k/min.
        let b = buckets(&[(120, 450_000), (2, 50_000)]);
        let g = compute(&b, NOW, &cfg(), None);
        let w = &g.window;
        assert_eq!(w.used, 500_000);
        assert!((w.rate_per_min - 10_000.0).abs() < 1.0);
        // cruise = 500k remaining / 180 min ≈ 2778/min → delta ≈ 3.6
        let delta = w.delta.unwrap();
        assert!((delta - 3.6).abs() < 0.1, "delta {delta}");
        // range = 500k / 10k = 50 min → wall long before the 3h reset.
        assert!((w.range_min.unwrap() - 50.0).abs() < 1.0);
        assert!(w.wall_at.is_some());
        // And that fires the alert.
        let alert = wall_alert(&g, NOW).expect("wall alert");
        assert!(alert.message.contains("~49m") || alert.message.contains("~50m"));
    }

    #[test]
    fn no_wall_when_coasting() {
        // Burning slower than cruise → no wall, delta < 1.
        let b = buckets(&[(120, 100_000), (2, 5_000)]);
        let g = compute(&b, NOW, &cfg(), None);
        assert!(g.window.delta.unwrap() < 1.0);
        assert!(g.window.wall_at.is_none());
        assert!(wall_alert(&g, NOW).is_none());
    }

    #[test]
    fn cruise_tank_rolls_hourly() {
        let b = buckets(&[(90, 500_000), (30, 100_000), (2, 50_000)]);
        let g = compute(&b, NOW, &cfg(), None);
        assert_eq!(g.cruise.used, 150_000, "only last-hour usage");
        assert_eq!(g.cruise.budget, Some(300_000));
        assert!(g.cruise.delta.unwrap() > 1.0, "10k/min vs 2.5k/min cruise");
    }

    #[test]
    fn learned_budget_from_429s() {
        // 429 fired 30 min ago, when the window (anchored 2h ago) had
        // accumulated 450k. That's the observed ceiling.
        let b = buckets(&[(120, 450_000), (2, 50_000)]);
        let rl = vec![NOW - 30 * 60_000];
        assert_eq!(learn_budget(&b, &rl, 5 * H), Some(450_000));

        // Learned feeds the tank when config has no budget.
        let mut c = cfg();
        c.governor_window_budget = None;
        let g = compute(&b, NOW, &c, Some(450_000));
        assert_eq!(g.window.budget, Some(450_000));
        assert_eq!(g.window.budget_source, BudgetSource::Learned);
    }

    #[test]
    fn empty_tank_is_infinite_delta() {
        let mut c = cfg();
        c.governor_window_budget = Some(100); // tiny budget, long overshot
        let b = buckets(&[(60, 500_000), (2, 50_000)]);
        let g = compute(&b, NOW, &c, None);
        assert!(g.window.delta.unwrap() >= DELTA_EMPTY);
        assert_eq!(g.window.range_min, Some(0.0));
    }

    #[test]
    fn merge_buckets_sums_same_slots() {
        let a = [(NOW, 100u64)];
        let b = [(NOW + 1, 200u64)]; // same 5-min bucket
        let merged = merge_buckets(&[&a[..], &b[..]]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].1, 300);
    }
}
