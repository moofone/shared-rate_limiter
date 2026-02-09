use std::time::Duration;

/// Slow-start ramp configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowStart {
    ramp: Duration,
}

impl SlowStart {
    /// Constructs a slow-start ramp with a non-zero duration.
    ///
    /// A zero duration is treated as "disabled" by callers (it would immediately jump to 100%).
    pub fn new(ramp: Duration) -> Self {
        Self { ramp }
    }

    /// Ramp duration.
    pub fn ramp(&self) -> Duration {
        self.ramp
    }
}

pub(crate) fn effective_max(
    max_per_window: u32,
    slow: SlowStart,
    started_at: Duration,
    now: Duration,
) -> u32 {
    if slow.ramp.is_zero() {
        return max_per_window;
    }
    let elapsed = now.saturating_sub(started_at);
    if elapsed >= slow.ramp {
        return max_per_window;
    }
    // floor(max * elapsed / ramp)
    let max_u128 = max_per_window as u128;
    let e = elapsed.as_nanos();
    let r = slow.ramp.as_nanos();
    ((max_u128 * e) / r) as u32
}

pub(crate) fn effective_u64(max: u64, slow: SlowStart, started_at: Duration, now: Duration) -> u64 {
    if slow.ramp.is_zero() {
        return max;
    }
    let elapsed = now.saturating_sub(started_at);
    if elapsed >= slow.ramp {
        return max;
    }
    let max_u128 = max as u128;
    let e = elapsed.as_nanos();
    let r = slow.ramp.as_nanos();
    let scaled = max_u128.saturating_mul(e) / r;
    scaled.min(u64::MAX as u128) as u64
}

pub(crate) fn retry_after_for_cost(
    max_per_window: u32,
    slow: SlowStart,
    started_at: Duration,
    now: Duration,
    cost: u32,
) -> Duration {
    if slow.ramp.is_zero() || max_per_window == 0 {
        return Duration::ZERO;
    }
    let elapsed = now.saturating_sub(started_at);
    if elapsed >= slow.ramp {
        return Duration::ZERO;
    }
    if cost > max_per_window {
        // Unachievable under the configured steady-state limit.
        return slow.ramp.saturating_sub(elapsed);
    }

    // Solve for the smallest elapsed `t` such that:
    //   floor(max * t / ramp) >= cost
    // which is satisfied when:
    //   t >= ceil(cost * ramp / max)
    let r = slow.ramp.as_nanos();
    let max = max_per_window as u128;
    let cost = cost as u128;
    let needed = (cost * r).div_ceil(max);
    let needed_dur = duration_from_nanos(needed);
    needed_dur.saturating_sub(elapsed)
}

pub(crate) fn retry_after_for_cost_u64(
    max: u64,
    slow: SlowStart,
    started_at: Duration,
    now: Duration,
    cost: u64,
) -> Duration {
    if slow.ramp.is_zero() || max == 0 {
        return Duration::ZERO;
    }
    let elapsed = now.saturating_sub(started_at);
    if elapsed >= slow.ramp {
        return Duration::ZERO;
    }
    if cost > max {
        return slow.ramp.saturating_sub(elapsed);
    }

    let r = slow.ramp.as_nanos();
    let max = max as u128;
    let cost = cost as u128;
    let needed = cost.saturating_mul(r).div_ceil(max);
    let needed_dur = duration_from_nanos(needed);
    needed_dur.saturating_sub(elapsed)
}

fn duration_from_nanos(n: u128) -> Duration {
    let secs_u128 = n / 1_000_000_000;
    if secs_u128 > u64::MAX as u128 {
        return Duration::MAX;
    }
    let secs = secs_u128 as u64;
    let nanos = (n % 1_000_000_000) as u32;
    Duration::new(secs, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_max_scales_linearly_and_floors() {
        let slow = SlowStart::new(Duration::from_secs(10));
        let started = Duration::from_secs(0);
        assert_eq!(effective_max(10, slow, started, Duration::from_secs(0)), 0);
        assert_eq!(effective_max(10, slow, started, Duration::from_secs(1)), 1);
        assert_eq!(effective_max(10, slow, started, Duration::from_secs(5)), 5);
        assert_eq!(effective_max(10, slow, started, Duration::from_secs(9)), 9);
        assert_eq!(
            effective_max(10, slow, started, Duration::from_secs(10)),
            10
        );
    }

    #[test]
    fn retry_after_for_cost_reaches_next_integer_step() {
        let slow = SlowStart::new(Duration::from_secs(10));
        let started = Duration::from_secs(0);
        // cost=1, max=10: need >=1s for effective_max >=1.
        assert_eq!(
            retry_after_for_cost(10, slow, started, Duration::from_secs(0), 1),
            Duration::from_secs(1)
        );
        // At t=5, cost=6 needs t>=6.
        assert_eq!(
            retry_after_for_cost(10, slow, started, Duration::from_secs(5), 6),
            Duration::from_secs(1)
        );
        assert_eq!(
            retry_after_for_cost(10, slow, started, Duration::from_secs(6), 6),
            Duration::from_secs(0)
        );
    }

    #[test]
    fn effective_u64_scales_linearly_and_floors() {
        let slow = SlowStart::new(Duration::from_secs(10));
        let started = Duration::from_secs(0);
        assert_eq!(effective_u64(10, slow, started, Duration::from_secs(0)), 0);
        assert_eq!(effective_u64(10, slow, started, Duration::from_secs(1)), 1);
        assert_eq!(
            effective_u64(10, slow, started, Duration::from_secs(10)),
            10
        );
    }
}
