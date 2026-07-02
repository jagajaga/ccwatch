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
use serde::{Deserialize, Serialize};

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

/// A 429 followed by at least this much usage silence is read as "we were
/// limited until the window reset" — the next activity anchors a NEW window.
const LIMIT_BLACKOUT_MS: i64 = 30 * 60 * 1000;

/// Find the current plan-window start.
///
/// Two signals, strongest first:
///
/// 1. **Rate-limit blackouts** (ground truth): a 429 followed by ≥30 min of
///    zero usage means the account was limited until Anthropic's window reset;
///    the first activity after the blackout anchors a fresh window. This
///    hard-corrects any drift from the replay below.
/// 2. **Chain replay**: windows are `window_ms` long, the first message starts
///    one, and after it expires the next message starts the next.
///
/// Returns `None` when there is no activity in the current window.
pub fn window_start(
    buckets: &[(i64, u64)],
    rate_limits: &[i64],
    now_ms: i64,
    window_ms: i64,
) -> Option<i64> {
    // Signal 1: the latest 429-blackout boundary.
    let mut hard_anchor: Option<i64> = None;
    for &rl in rate_limits {
        if rl > now_ms {
            continue;
        }
        // First activity strictly after the 429.
        let resume = buckets
            .iter()
            .find(|(ts, v)| *v > 0 && *ts > bucket_of(rl))
            .map(|(ts, _)| *ts);
        if let Some(r) = resume {
            if r - rl >= LIMIT_BLACKOUT_MS && hard_anchor.map(|h| r > h).unwrap_or(true) {
                hard_anchor = Some(r);
            }
        }
    }

    // Signal 2: chain replay, starting from the hard anchor when we have one.
    let mut start: Option<i64> = hard_anchor;
    for &(ts, v) in buckets {
        if v == 0 || ts > now_ms {
            continue;
        }
        if let Some(h) = hard_anchor {
            if ts < h {
                continue; // pre-boundary history is a previous window
            }
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

/// Compute the full governor status. `rate_limits` are observed 429 timestamps
/// (they hard-anchor window boundaries); `learned_window_budget` comes from
/// them too and is used when no config budget is set.
pub fn compute(
    buckets: &[(i64, u64)],
    rate_limits: &[i64],
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
    let window = match window_start(buckets, rate_limits, now_ms, window_ms) {
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

/// The persisted budget estimate. `hard` means it was *measured* at a
/// confirmed wall (429 + blackout); soft evidence only bounds from below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedBudget {
    pub tokens: u64,
    pub hard: bool,
    /// When the evidence was observed (epoch ms) — newest hard wall wins.
    pub at_ms: i64,
}

/// Update the budget estimate from the recent horizon. The rules make plan
/// changes self-correcting in both directions:
///
/// - a **confirmed wall** (429 followed by ≥30 min of silence) *measures* the
///   budget: the newest wall SETS it — up or **down** (plan downgrades);
/// - a transient 429 or plain observed usage can only RAISE it (you provably
///   had at least that much) — never lower it.
pub fn learn(
    buckets: &[(i64, u64)],
    rate_limits: &[i64],
    window_ms: i64,
    now_ms: i64,
    prev: Option<LearnedBudget>,
) -> Option<LearnedBudget> {
    let mut best = prev;
    let mut rls: Vec<i64> = rate_limits.to_vec();
    rls.sort_unstable();

    for &rl in &rls {
        if rl > now_ms {
            continue;
        }
        let resume = buckets
            .iter()
            .find(|(ts, v)| *v > 0 && *ts > bucket_of(rl))
            .map(|(ts, _)| *ts);
        let hard = match resume {
            Some(r) => r - rl >= LIMIT_BLACKOUT_MS,
            None => now_ms - rl >= LIMIT_BLACKOUT_MS, // still blacked out
        };
        // Usage of the window that was active when the 429 fired; anchor it
        // only with boundaries that existed before this event.
        let prior: Vec<i64> = rls.iter().copied().filter(|t| *t < rl).collect();
        let Some(start) = window_start(buckets, &prior, rl, window_ms) else {
            continue;
        };
        let used = sum_range(buckets, start, rl);
        if used == 0 {
            continue;
        }
        if hard {
            let newer = best.map(|b| !b.hard || rl > b.at_ms).unwrap_or(true);
            if newer {
                best = Some(LearnedBudget {
                    tokens: used,
                    hard: true,
                    at_ms: rl,
                });
            }
        } else if best.map(|b| used > b.tokens).unwrap_or(true) {
            // Transient 429: a lower bound only.
            let at = best.map(|b| b.at_ms).unwrap_or(rl);
            let hard_kept = false;
            best = Some(LearnedBudget {
                tokens: used,
                hard: hard_kept,
                at_ms: at,
            });
        }
    }

    // If the current window's usage exceeds the estimate, the estimate was
    // too small (e.g. after a plan upgrade with no wall hit yet).
    if let Some(start) = window_start(buckets, &rls, now_ms, window_ms) {
        let used = sum_range(buckets, start, now_ms);
        match &mut best {
            Some(b) if used > b.tokens => b.tokens = used,
            None if used > 0 => {} // usage alone isn't a budget estimate
            _ => {}
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
        let start = window_start(&b, &[], NOW, 5 * H).unwrap();
        assert_eq!(start, bucket_of(NOW - 30 * 60_000));

        // No activity in the current window → None.
        let b = buckets(&[(360, 100)]);
        assert!(window_start(&b, &[], NOW, 5 * H).is_none());
    }

    #[test]
    fn rate_limit_blackout_hard_anchors_the_window() {
        // The user's real scenario: burning 20:00→22:25, hit the limit
        // (429s) at 22:25, blacked out until 01:05, resumed, now it's 02:30.
        // Naive chain replay from truncated history would anchor wrong; the
        // 429+blackout must anchor the window at the 01:05 resume.
        //
        // Offsets from NOW (=02:30): 20:00=390m, 22:25=245m, 01:05=85m.
        let b = buckets(&[
            (390, 500_000), // evening burn
            (300, 800_000),
            (250, 900_000), // approaching the wall
            (85, 50_000),   // resumed after unblock
            (30, 80_000),
            (2, 40_000),
        ]);
        let rl = vec![NOW - 245 * 60_000]; // 429 at 22:25
        let start = window_start(&b, &rl, NOW, 5 * H).unwrap();
        assert_eq!(
            start,
            bucket_of(NOW - 85 * 60_000),
            "window must anchor at the post-blackout resume"
        );

        // Used counts only post-resume burn.
        let mut c = cfg();
        c.governor_window_budget = None;
        let g = compute(&b, &rl, NOW, &c, None);
        assert_eq!(g.window.used, 170_000);
        // Reset = resume + 5h.
        assert_eq!(g.window.resets_at, Some(bucket_of(NOW - 85 * 60_000) + 5 * H));

        // A transient 429 with immediate continuation must NOT re-anchor.
        let b2 = buckets(&[(120, 100_000), (110, 100_000), (2, 50_000)]);
        let rl2 = vec![NOW - 115 * 60_000]; // 429 mid-burn, work continued
        let start2 = window_start(&b2, &rl2, NOW, 5 * H).unwrap();
        assert_eq!(start2, bucket_of(NOW - 120 * 60_000), "no blackout → no re-anchor");
    }

    #[test]
    fn used_counts_only_current_window() {
        // Old window: activity 7h ago (window expired 2h ago). Current window
        // anchors at 80m ago; the 30m-ago burn is inside it too.
        let b = buckets(&[(420, 100_000), (80, 200_000), (30, 300_000)]);
        let g = compute(&b, &[], NOW, &cfg(), None);
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
        let g = compute(&b, &[], NOW, &cfg(), None);
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
        let g = compute(&b, &[], NOW, &cfg(), None);
        assert!(g.window.delta.unwrap() < 1.0);
        assert!(g.window.wall_at.is_none());
        assert!(wall_alert(&g, NOW).is_none());
    }

    #[test]
    fn cruise_tank_rolls_hourly() {
        let b = buckets(&[(90, 500_000), (30, 100_000), (2, 50_000)]);
        let g = compute(&b, &[], NOW, &cfg(), None);
        assert_eq!(g.cruise.used, 150_000, "only last-hour usage");
        assert_eq!(g.cruise.budget, Some(300_000));
        assert!(g.cruise.delta.unwrap() > 1.0, "10k/min vs 2.5k/min cruise");
    }

    #[test]
    fn learned_budget_from_429s() {
        // Transient 429 30 min ago at 450k window usage → soft lower bound
        // (work resumed 5 min later — a different bucket, no blackout).
        let b = buckets(&[(120, 450_000), (25, 50_000)]);
        let rl = vec![NOW - 30 * 60_000];
        let learned = learn(&b, &rl, 5 * H, NOW, None).unwrap();
        assert_eq!(learned.tokens, 500_000, "current usage raises the bound");
        assert!(!learned.hard, "no blackout → soft evidence");

        // Learned feeds the tank when config has no budget.
        let mut c = cfg();
        c.governor_window_budget = None;
        let g = compute(&b, &rl, NOW, &c, Some(learned.tokens));
        assert_eq!(g.window.budget, Some(500_000));
        assert_eq!(g.window.budget_source, BudgetSource::Learned);
    }

    #[test]
    fn plan_downgrade_lowers_budget_on_confirmed_wall() {
        // Old plan: burned 20M, hit a wall (429 + 65min blackout) 265m ago.
        // New (downgraded) plan: resumed 200m ago, burned only 12M, hit a
        // wall again 168m ago, blacked out since. The newest confirmed wall
        // must SET the budget DOWN to 12M.
        let b = buckets(&[
            (290, 8_000_000),
            (275, 12_000_000), // 20M by the first wall
            (200, 5_000_000),
            (180, 7_000_000), // 12M by the second wall
        ]);
        let rls = vec![NOW - 265 * 60_000, NOW - 168 * 60_000];

        // Even starting from a stale, too-big estimate:
        let prev = Some(LearnedBudget { tokens: 23_700_000, hard: true, at_ms: NOW - 3000 * 60_000 });
        let learned = learn(&b, &rls, 5 * H, NOW, prev).unwrap();
        assert_eq!(learned.tokens, 12_000_000, "downgrade must lower the budget");
        assert!(learned.hard);
        assert_eq!(learned.at_ms, NOW - 168 * 60_000);

        // A later transient 429 at tiny usage must NOT lower it further.
        let b2 = buckets(&[(20, 300_000), (16, 100_000)]);
        let rl2 = vec![NOW - 18 * 60_000]; // resumed 2 min later → soft
        let after = learn(&b2, &rl2, 5 * H, NOW, Some(learned)).unwrap();
        assert_eq!(after.tokens, 12_000_000, "soft 429 never lowers");
        assert!(after.hard);

        // But usage EXCEEDING the estimate raises it (plan upgraded).
        let b3 = buckets(&[(60, 14_000_000)]);
        let up = learn(&b3, &[], 5 * H, NOW, Some(learned)).unwrap();
        assert_eq!(up.tokens, 14_000_000, "observed usage raises the floor");
    }

    #[test]
    fn empty_tank_is_infinite_delta() {
        let mut c = cfg();
        c.governor_window_budget = Some(100); // tiny budget, long overshot
        let b = buckets(&[(60, 500_000), (2, 50_000)]);
        let g = compute(&b, &[], NOW, &c, None);
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
