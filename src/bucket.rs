use std::time::Duration;

use crate::config::AlgorithmConfig;
use crate::token_bucket::TokenBucketState;
use crate::types::{Cost, Deny, Outcome, Permit, PermitMeta, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BucketDenyReason {
    BlockedUntil,
    WindowExhausted,
    Disabled,
    Unachievable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BucketDeny {
    pub(crate) deny: Deny,
    pub(crate) reason: BucketDenyReason,
}

#[derive(Debug, Clone)]
struct FixedWindowState {
    window_start: Duration,
    reserved: u32,
    spent: u32,
}

impl FixedWindowState {
    fn new(now: Duration, window: Duration) -> Self {
        Self {
            window_start: window_start_for(now, window),
            reserved: 0,
            spent: 0,
        }
    }

    fn has_outstanding(&self) -> bool {
        self.reserved != 0
    }

    fn maybe_reset_window(&mut self, now: Duration, window: Duration) {
        // If there are outstanding reservations, we do not advance windows. This prevents
        // permits from being used "after" the limiter has reset its accounting.
        if self.reserved != 0 {
            return;
        }

        let end = self.window_start + window;
        if now >= end {
            self.window_start = window_start_for(now, window);
            self.spent = 0;
            self.reserved = 0;
        }
    }

    fn retry_after_for_window(&self, now: Duration, window: Duration) -> Duration {
        let end = self.window_start + window;
        end.saturating_sub(now)
    }
}

#[derive(Debug, Clone)]
enum AlgoState {
    FixedWindow(FixedWindowState),
    TokenBucket(TokenBucketState),
}

#[derive(Debug, Clone)]
pub(crate) struct BucketState {
    blocked_until: Option<Duration>,
    algo: AlgoState,
}

impl BucketState {
    pub(crate) fn new(now: Duration, algo: AlgorithmConfig) -> Self {
        let state = match algo {
            AlgorithmConfig::FixedWindow { window, .. } => {
                AlgoState::FixedWindow(FixedWindowState::new(now, window))
            }
            AlgorithmConfig::TokenBucket { capacity, .. } => {
                AlgoState::TokenBucket(TokenBucketState::new(now, capacity))
            }
        };
        Self {
            blocked_until: None,
            algo: state,
        }
    }

    pub(crate) fn has_outstanding(&self) -> bool {
        match &self.algo {
            AlgoState::FixedWindow(s) => s.has_outstanding(),
            AlgoState::TokenBucket(s) => s.has_outstanding(),
        }
    }

    pub(crate) fn try_reserve(
        &mut self,
        key: crate::types::Key,
        algo: AlgorithmConfig,
        cost: Cost,
        now: Duration,
    ) -> Result<Permit, BucketDeny> {
        self.clear_expired_block(now);

        if let Some(until) = self.blocked_until
            && now < until
        {
            return Err(BucketDeny {
                deny: Deny {
                    retry_after: until - now,
                },
                reason: BucketDenyReason::BlockedUntil,
            });
        }

        match (&mut self.algo, algo) {
            (
                AlgoState::FixedWindow(s),
                AlgorithmConfig::FixedWindow {
                    max_per_window,
                    window,
                },
            ) => {
                s.maybe_reset_window(now, window);

                if max_per_window == 0 {
                    return Err(BucketDeny {
                        deny: Deny {
                            retry_after: window,
                        },
                        reason: BucketDenyReason::Disabled,
                    });
                }

                let cost_u32 = cost.get();
                let in_use = s.spent.saturating_add(s.reserved);
                let available = max_per_window.saturating_sub(in_use);
                if cost_u32 > available {
                    return Err(BucketDeny {
                        deny: Deny {
                            retry_after: s.retry_after_for_window(now, window),
                        },
                        reason: BucketDenyReason::WindowExhausted,
                    });
                }

                s.reserved = s.reserved.saturating_add(cost_u32);

                let deadline = s.window_start + window;
                Ok(Permit {
                    key,
                    cost,
                    window_start: s.window_start,
                    issued_at: now,
                    deadline,
                    meta: PermitMeta::FixedWindow {
                        window_start: s.window_start,
                    },
                })
            }

            (
                AlgoState::TokenBucket(s),
                AlgorithmConfig::TokenBucket {
                    capacity,
                    refill_per_sec,
                },
            ) => {
                if capacity == 0 {
                    return Err(BucketDeny {
                        deny: Deny {
                            retry_after: Duration::MAX,
                        },
                        reason: BucketDenyReason::Disabled,
                    });
                }

                match s.try_reserve(capacity, refill_per_sec, cost, now) {
                    Ok(()) => Ok(Permit {
                        key,
                        cost,
                        window_start: Duration::ZERO,
                        issued_at: now,
                        deadline: Duration::ZERO,
                        meta: PermitMeta::TokenBucket,
                    }),
                    Err(deny) => Err(BucketDeny {
                        deny,
                        reason: if deny.retry_after == Duration::MAX {
                            BucketDenyReason::Unachievable
                        } else {
                            BucketDenyReason::WindowExhausted
                        },
                    }),
                }
            }

            // Config algorithm changed without refreshing bucket state; safe no-op path.
            (AlgoState::FixedWindow(_), AlgorithmConfig::TokenBucket { .. })
            | (AlgoState::TokenBucket(_), AlgorithmConfig::FixedWindow { .. }) => Err(BucketDeny {
                deny: Deny {
                    retry_after: Duration::from_secs(1),
                },
                reason: BucketDenyReason::Disabled,
            }),
        }
    }

    pub(crate) fn refund(&mut self, permit: Permit, now: Duration, algo: AlgorithmConfig) {
        self.clear_expired_block(now);
        match (&mut self.algo, algo, permit.meta) {
            (
                AlgoState::FixedWindow(s),
                AlgorithmConfig::FixedWindow { window, .. },
                PermitMeta::FixedWindow { window_start },
            ) => {
                s.maybe_reset_window(now, window);
                if window_start != s.window_start {
                    debug_assert!(
                        false,
                        "refund for permit in a different window; this indicates late finalization"
                    );
                    return;
                }
                s.reserved = s.reserved.saturating_sub(permit.cost.get());
            }
            (
                AlgoState::TokenBucket(s),
                AlgorithmConfig::TokenBucket {
                    capacity,
                    refill_per_sec,
                },
                PermitMeta::TokenBucket,
            ) => {
                s.refund(capacity, permit.cost, now, refill_per_sec);
            }
            _ => {
                debug_assert!(false, "refund for permit with mismatched algorithm/config");
            }
        }
    }

    pub(crate) fn commit(
        &mut self,
        permit: Permit,
        outcome: Outcome,
        now: Duration,
        algo: AlgorithmConfig,
    ) {
        self.clear_expired_block(now);
        match outcome {
            Outcome::RateLimitedFeedback {
                retry_after,
                scope: Scope::Key,
                ..
            } => {
                self.apply_blocked_until(now + retry_after);
            }
            Outcome::RateLimitedFeedback {
                scope: Scope::Global,
                ..
            } => {
                // Global scope is handled by the outer RateLimiter.
            }
            _ => {}
        }

        match (&mut self.algo, algo, permit.meta) {
            (
                AlgoState::FixedWindow(s),
                AlgorithmConfig::FixedWindow { window, .. },
                PermitMeta::FixedWindow { window_start },
            ) => {
                s.maybe_reset_window(now, window);
                if window_start != s.window_start {
                    debug_assert!(
                        false,
                        "commit for permit in a different window; this indicates late finalization"
                    );
                    return;
                }

                // Common case: commit consumes the reservation but does not restore capacity.
                s.reserved = s.reserved.saturating_sub(permit.cost.get());
                s.spent = s.spent.saturating_add(permit.cost.get());

                match outcome {
                    Outcome::NotSent => {
                        // Defensive: treat misclassified NotSent as a refund.
                        s.spent = s.spent.saturating_sub(permit.cost.get());
                    }
                    Outcome::Confirmed
                    | Outcome::SentNoConfirm
                    | Outcome::RateLimitedFeedback { .. } => {}
                }
            }
            (
                AlgoState::TokenBucket(s),
                AlgorithmConfig::TokenBucket {
                    capacity,
                    refill_per_sec,
                },
                PermitMeta::TokenBucket,
            ) => match outcome {
                Outcome::NotSent => s.refund(capacity, permit.cost, now, refill_per_sec),
                _ => s.commit(permit.cost),
            },
            _ => {
                debug_assert!(false, "commit for permit with mismatched algorithm/config");
            }
        }
    }

    pub(crate) fn apply_blocked_until(&mut self, until: Duration) {
        self.blocked_until = Some(self.blocked_until.map_or(until, |cur| cur.max(until)));
    }

    pub(crate) fn on_update(&mut self, old: AlgorithmConfig, new: AlgorithmConfig, now: Duration) {
        self.clear_expired_block(now);
        let has_outstanding = self.has_outstanding();
        match (&mut self.algo, old, new) {
            (
                AlgoState::FixedWindow(s),
                AlgorithmConfig::FixedWindow { window: old_w, .. },
                AlgorithmConfig::FixedWindow { window: new_w, .. },
            ) => {
                // If the window changed, re-anchor window_start but keep outstanding reservations.
                if old_w != new_w && !s.has_outstanding() {
                    s.window_start = window_start_for(now, new_w);
                    s.spent = 0;
                } else if old_w != new_w && s.has_outstanding() {
                    debug_assert!(
                        false,
                        "fixed-window window changed with outstanding permits; late finalization will be a no-op"
                    );
                }
            }
            (
                AlgoState::TokenBucket(s),
                AlgorithmConfig::TokenBucket {
                    capacity: old_cap,
                    refill_per_sec: old_refill,
                },
                AlgorithmConfig::TokenBucket {
                    capacity: new_cap,
                    refill_per_sec: _new_refill,
                },
            ) => {
                // Bring credits current under the old rate, then clamp to new capacity.
                s.refill(old_cap, old_refill, now);
                s.apply_capacity_change(old_cap, new_cap);
            }
            (state, _old, new_algo) => {
                if has_outstanding {
                    debug_assert!(
                        false,
                        "algorithm changed with outstanding permits; late finalization will be a no-op"
                    );
                }
                *state = match new_algo {
                    AlgorithmConfig::FixedWindow { window, .. } => {
                        AlgoState::FixedWindow(FixedWindowState::new(now, window))
                    }
                    AlgorithmConfig::TokenBucket { capacity, .. } => {
                        AlgoState::TokenBucket(TokenBucketState::new(now, capacity))
                    }
                };
            }
        }
    }

    fn clear_expired_block(&mut self, now: Duration) {
        if let Some(until) = self.blocked_until
            && now >= until
        {
            self.blocked_until = None;
        }
    }
}

pub(crate) fn window_start_for(now: Duration, window: Duration) -> Duration {
    debug_assert!(!window.is_zero());
    let w = window.as_nanos();
    let n = now.as_nanos();
    // Floor division: start = (n / w) * w
    let start = (n / w) * w;
    // Convert u128 nanoseconds back to Duration without truncation.
    let secs = (start / 1_000_000_000) as u64;
    let nanos = (start % 1_000_000_000) as u32;
    Duration::new(secs, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Key;

    #[test]
    fn fixed_window_retry_after_decreases_over_time() {
        let cfg = AlgorithmConfig::FixedWindow {
            max_per_window: 2,
            window: Duration::from_secs(10),
        };
        let mut b = BucketState::new(Duration::from_secs(0), cfg);

        let _p1 = b
            .try_reserve(1, cfg, Cost::ONE, Duration::from_secs(0))
            .unwrap();
        let _p2 = b
            .try_reserve(1, cfg, Cost::ONE, Duration::from_secs(0))
            .unwrap();

        let d1 = b
            .try_reserve(1, cfg, Cost::ONE, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(d1.deny.retry_after, Duration::from_secs(9));

        let d2 = b
            .try_reserve(1, cfg, Cost::ONE, Duration::from_secs(9))
            .unwrap_err();
        assert_eq!(d2.deny.retry_after, Duration::from_secs(1));
    }

    #[test]
    fn token_bucket_blocks_until_refill() {
        let cfg = AlgorithmConfig::TokenBucket {
            capacity: 10,
            refill_per_sec: 10,
        };
        let mut b = BucketState::new(Duration::from_secs(0), cfg);
        let p = b
            .try_reserve(1, cfg, Cost::new(10).unwrap(), Duration::from_secs(0))
            .unwrap();
        b.commit(p, Outcome::SentNoConfirm, Duration::from_secs(0), cfg);

        let d = b
            .try_reserve(1, cfg, Cost::ONE, Duration::from_secs(0))
            .unwrap_err();
        assert_eq!(d.deny.retry_after, Duration::from_millis(100));
    }

    #[test]
    fn blocked_until_denies_before_algorithm_logic() {
        let cfg = AlgorithmConfig::FixedWindow {
            max_per_window: 10,
            window: Duration::from_secs(10),
        };
        let mut b = BucketState::new(Duration::from_secs(0), cfg);
        b.apply_blocked_until(Duration::from_secs(5));
        let d = b
            .try_reserve(Key::MAX, cfg, Cost::ONE, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(d.reason, BucketDenyReason::BlockedUntil);
        assert_eq!(d.deny.retry_after, Duration::from_secs(4));
    }
}
