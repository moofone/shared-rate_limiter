use std::time::Duration;

use crate::types::{Cost, Deny};

#[derive(Debug, Clone)]
pub(crate) struct TokenBucketState {
    last_update: Duration,
    credits: u64,
    in_flight: u64,
}

impl TokenBucketState {
    pub(crate) fn new(now: Duration, capacity: u64) -> Self {
        Self {
            last_update: now,
            credits: capacity,
            in_flight: 0,
        }
    }

    pub(crate) fn has_outstanding(&self) -> bool {
        self.in_flight != 0
    }

    pub(crate) fn try_reserve(
        &mut self,
        capacity: u64,
        refill_per_sec: u64,
        cost: Cost,
        now: Duration,
    ) -> Result<(), Deny> {
        self.refill(capacity, refill_per_sec, now);

        let want = cost.get() as u64;
        if want == 0 {
            debug_assert!(false, "Cost is NonZero");
            return Ok(());
        }
        if want > capacity {
            return Err(Deny {
                retry_after: Duration::MAX,
            });
        }
        if self.credits >= want {
            self.credits -= want;
            self.in_flight = self.in_flight.saturating_add(want);
            return Ok(());
        }

        if refill_per_sec == 0 {
            return Err(Deny {
                retry_after: Duration::MAX,
            });
        }

        let deficit = want - self.credits;
        let retry_after = retry_after_for_deficit(deficit, refill_per_sec);
        Err(Deny { retry_after })
    }

    pub(crate) fn refund(&mut self, capacity: u64, cost: Cost, now: Duration, refill_per_sec: u64) {
        self.refill(capacity, refill_per_sec, now);
        let amt = cost.get() as u64;
        self.in_flight = self.in_flight.saturating_sub(amt);
        self.credits = (self.credits + amt).min(capacity);
    }

    pub(crate) fn commit(&mut self, cost: Cost) {
        let amt = cost.get() as u64;
        self.in_flight = self.in_flight.saturating_sub(amt);
    }

    pub(crate) fn apply_capacity_change(&mut self, old_capacity: u64, new_capacity: u64) {
        if new_capacity >= old_capacity {
            // Preserve the current "deficit" when the max pool increases.
            let delta = new_capacity - old_capacity;
            self.credits = (self.credits.saturating_add(delta)).min(new_capacity);
        } else {
            self.credits = self.credits.min(new_capacity);
        }
    }

    pub(crate) fn refill(&mut self, capacity: u64, refill_per_sec: u64, now: Duration) {
        if now <= self.last_update {
            return;
        }
        if refill_per_sec == 0 || capacity == 0 {
            self.last_update = now;
            self.credits = self.credits.min(capacity);
            return;
        }

        let delta = now - self.last_update;
        let add = credits_added(delta, refill_per_sec);
        self.credits = (self.credits.saturating_add(add)).min(capacity);
        self.last_update = now;
    }
}

fn credits_added(delta: Duration, refill_per_sec: u64) -> u64 {
    // floor(refill_per_sec * delta_nanos / 1e9)
    let nanos = delta.as_nanos();
    let add = (nanos.saturating_mul(refill_per_sec as u128)) / 1_000_000_000u128;
    add.min(u64::MAX as u128) as u64
}

fn retry_after_for_deficit(deficit: u64, refill_per_sec: u64) -> Duration {
    // ceil(deficit / refill_per_sec) seconds with nanos precision:
    // nanos = ceil(deficit * 1e9 / refill_per_sec)
    let num = (deficit as u128).saturating_mul(1_000_000_000u128);
    let denom = refill_per_sec as u128;
    let nanos = num.div_ceil(denom);
    let secs = (nanos / 1_000_000_000) as u64;
    let sub = (nanos % 1_000_000_000) as u32;
    Duration::new(secs, sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Cost;

    #[test]
    fn basic_burst_then_deny() {
        let mut s = TokenBucketState::new(Duration::from_secs(0), 10);
        for _ in 0..10 {
            s.try_reserve(10, 1, Cost::ONE, Duration::from_secs(0))
                .unwrap();
            s.commit(Cost::ONE);
        }
        let d = s
            .try_reserve(10, 1, Cost::ONE, Duration::from_secs(0))
            .unwrap_err();
        assert!(d.retry_after > Duration::ZERO);
    }

    #[test]
    fn refill_adds_credits_up_to_capacity() {
        let mut s = TokenBucketState::new(Duration::from_secs(0), 10);
        s.try_reserve(10, 10, Cost::new(10).unwrap(), Duration::from_secs(0))
            .unwrap();
        s.commit(Cost::new(10).unwrap());
        assert_eq!(s.credits, 0);

        s.refill(10, 10, Duration::from_millis(500));
        assert_eq!(s.credits, 5);
        s.refill(10, 10, Duration::from_secs(2));
        assert_eq!(s.credits, 10);
    }

    #[test]
    fn weighted_cost_consumes_proportionally() {
        let mut s = TokenBucketState::new(Duration::from_secs(0), 10);
        s.try_reserve(10, 10, Cost::new(7).unwrap(), Duration::from_secs(0))
            .unwrap();
        assert_eq!(s.credits, 3);
    }

    #[test]
    fn retry_after_is_minimal_delay_until_enough() {
        let mut s = TokenBucketState::new(Duration::from_secs(0), 10);
        s.try_reserve(10, 10, Cost::new(10).unwrap(), Duration::from_secs(0))
            .unwrap();
        s.commit(Cost::new(10).unwrap());
        let d = s
            .try_reserve(10, 10, Cost::ONE, Duration::from_secs(0))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::from_millis(100));
    }

    #[test]
    fn zero_refill_is_permanent_after_exhaustion() {
        let mut s = TokenBucketState::new(Duration::from_secs(0), 2);
        s.try_reserve(2, 0, Cost::ONE, Duration::from_secs(0))
            .unwrap();
        s.commit(Cost::ONE);
        s.try_reserve(2, 0, Cost::ONE, Duration::from_secs(0))
            .unwrap();
        s.commit(Cost::ONE);
        let d = s
            .try_reserve(2, 0, Cost::ONE, Duration::from_secs(0))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::MAX);
    }

    #[test]
    fn cost_above_capacity_is_permanent_deny() {
        let mut s = TokenBucketState::new(Duration::from_secs(0), 2);
        let d = s
            .try_reserve(2, 10, Cost::new(3).unwrap(), Duration::from_secs(0))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::MAX);
    }
}
