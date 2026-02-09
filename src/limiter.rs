use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use crate::bucket::{BucketDenyReason, BucketState};
use crate::config::{AlgorithmConfig, Config, OverflowPolicy};
use crate::feedback::{FeedbackScope, RateLimitFeedback};
use crate::observer::{DenyEvent, DenyReason, FeedbackEvent, LimiterObserver, NoopObserver};
use crate::slow_start;
use crate::types::{Cost, Deny, Key, Outcome, Permit, Scope};

/// A protocol-agnostic, synchronous rate limiter.
///
/// This core type is intentionally single-owner (`&mut self` methods) to keep the hot path fast
/// and allocation-free for existing keys. Concurrency is achieved by wrapping it in an actor or
/// by putting it behind an external synchronization strategy chosen by the application.
pub struct RateLimiter {
    cfg: Config,
    started_at: Duration,

    // Input (route) key -> bucket key remap. Defaults to identity when absent.
    route_map: HashMap<Key, Key>,

    buckets: HashMap<Key, BucketState>,
    fifo: VecDeque<Key>,

    global_blocked_until: Option<Duration>,

    observer: Box<dyn LimiterObserver>,
}

impl RateLimiter {
    /// Create a new limiter with `started_at = Duration::ZERO`.
    pub fn new(cfg: Config) -> Self {
        Self::new_at(cfg, Duration::ZERO)
    }

    /// Create a new limiter with an explicit `started_at` timestamp (must use the same clock domain as `now`).
    pub fn new_at(cfg: Config, started_at: Duration) -> Self {
        Self {
            cfg,
            started_at,
            route_map: HashMap::new(),
            buckets: HashMap::new(),
            fifo: VecDeque::new(),
            global_blocked_until: None,
            observer: Box::new(NoopObserver),
        }
    }

    /// Attach an observer to receive deny/feedback events.
    pub fn set_observer(&mut self, observer: Box<dyn LimiterObserver>) {
        self.observer = observer;
    }

    /// Attempt to reserve `cost` capacity for `route_key` at time `now`.
    pub fn try_acquire(
        &mut self,
        route_key: Key,
        cost: Cost,
        now: Duration,
    ) -> Result<Permit, Deny> {
        let bucket_key = self.resolve_bucket_key(route_key);

        if let Some(until) = self.global_blocked_until
            && now < until
        {
            let deny = Deny {
                retry_after: until - now,
            };
            self.observer.on_deny(DenyEvent {
                route_key,
                bucket_key,
                cost,
                now,
                deny,
                reason: DenyReason::FeedbackBackoff,
            });
            return Err(deny);
        }

        let effective_algo = self.effective_algorithm(now);
        if let Some(deny) = self.slow_start_pre_deny(cost, now, effective_algo) {
            self.observer.on_deny(DenyEvent {
                route_key,
                bucket_key,
                cost,
                now,
                deny,
                reason: DenyReason::SlowStart,
            });
            return Err(deny);
        }

        // Ensure the bucket exists; for existing buckets, this is allocation-free.
        self.ensure_bucket_capacity(route_key, bucket_key, cost, now)?;

        let res = match self.buckets.entry(bucket_key) {
            Entry::Occupied(mut e) => {
                e.get_mut()
                    .try_reserve(bucket_key, effective_algo, cost, now)
            }
            Entry::Vacant(v) => {
                self.fifo.push_back(bucket_key);
                v.insert(BucketState::new(now, self.cfg.algorithm))
                    .try_reserve(bucket_key, effective_algo, cost, now)
            }
        };

        match res {
            Ok(p) => Ok(p),
            Err(d) => {
                let reason = match d.reason {
                    BucketDenyReason::BlockedUntil => DenyReason::FeedbackBackoff,
                    BucketDenyReason::WindowExhausted => DenyReason::WindowExhausted,
                    BucketDenyReason::Disabled => DenyReason::Disabled,
                    BucketDenyReason::Unachievable => DenyReason::Disabled,
                };
                self.observer.on_deny(DenyEvent {
                    route_key,
                    bucket_key,
                    cost,
                    now,
                    deny: d.deny,
                    reason,
                });
                Err(d.deny)
            }
        }
    }

    /// Apply a server rate-limit feedback signal.
    ///
    /// This is intended for 429-like feedback paths and does not require a `Permit`.
    pub fn apply_feedback<F: RateLimitFeedback>(
        &mut self,
        route_key: Key,
        feedback: &F,
        now: Duration,
    ) {
        let remap = feedback.bucket_remap();
        let applied_remap = remap.and_then(|new_bucket| {
            if self.apply_route_remap(route_key, new_bucket) {
                Some(new_bucket)
            } else {
                None
            }
        });

        let bucket_key = self.resolve_bucket_key(route_key);

        let mut backoff = feedback.retry_after();
        if feedback.remaining() == Some(0)
            && let Some(reset_after) = feedback.reset_after()
        {
            backoff = backoff.max(reset_after);
        }
        let until = now.saturating_add(backoff);

        match feedback.scope() {
            FeedbackScope::Global => {
                self.global_blocked_until = Some(
                    self.global_blocked_until
                        .map_or(until, |cur| cur.max(until)),
                );
            }
            FeedbackScope::Key | FeedbackScope::Route => {
                // Ensure bucket exists deterministically for unknown keys.
                if let Some(b) = self.bucket_mut_or_insert(route_key, bucket_key, now) {
                    b.apply_blocked_until(until);
                }
            }
        }

        self.observer.on_feedback(FeedbackEvent {
            route_key,
            bucket_key,
            now,
            retry_after: backoff,
            scope: feedback.scope(),
            bucket_remap: applied_remap,
        });
    }

    /// Finalize a permit as spent.
    ///
    /// If `outcome` is [`Outcome::NotSent`], this acts like a refund.
    pub fn commit(&mut self, permit: Permit, outcome: Outcome, now: Duration) {
        if let Outcome::RateLimitedFeedback {
            retry_after,
            scope: Scope::Global,
            ..
        } = &outcome
        {
            let until = now.saturating_add(*retry_after);
            self.global_blocked_until = Some(
                self.global_blocked_until
                    .map_or(until, |cur| cur.max(until)),
            );
        }

        if let Some(bucket) = self.buckets.get_mut(&permit.key) {
            bucket.commit(permit, outcome, now, self.cfg.algorithm);
        }
    }

    /// Finalize a permit as not-sent and restore capacity.
    pub fn refund(&mut self, permit: Permit, now: Duration) {
        if let Some(bucket) = self.buckets.get_mut(&permit.key) {
            bucket.refund(permit, now, self.cfg.algorithm);
        }
    }

    /// Apply a runtime configuration update deterministically.
    ///
    /// This does not invalidate outstanding permits, but if the algorithm kind changes while permits are outstanding,
    /// finalization may be treated as a no-op (debug asserted) to preserve safety.
    pub fn update_config(&mut self, new_cfg: Config, now: Duration) {
        let old_algo = self.cfg.algorithm;
        let new_algo = new_cfg.algorithm;
        self.cfg = new_cfg;
        for b in self.buckets.values_mut() {
            b.on_update(old_algo, new_algo, now);
        }
    }

    /// Current number of tracked bucket keys.
    pub fn tracked_keys(&self) -> usize {
        self.buckets.len()
    }

    fn resolve_bucket_key(&self, route_key: Key) -> Key {
        self.route_map.get(&route_key).copied().unwrap_or(route_key)
    }

    fn apply_route_remap(&mut self, route_key: Key, new_bucket: Key) -> bool {
        // Best-effort bounding: reuse `max_keys` if configured.
        if let Some(max_keys) = self.cfg.max_keys
            && self.route_map.len() >= max_keys
            && !self.route_map.contains_key(&route_key)
        {
            // If the mapping table is full, refuse to grow it; the caller can still pass the bucket key directly.
            return false;
        }
        self.route_map.insert(route_key, new_bucket);
        true
    }

    fn ensure_bucket_capacity(
        &mut self,
        route_key: Key,
        bucket_key: Key,
        cost: Cost,
        now: Duration,
    ) -> Result<(), Deny> {
        let retry_after = self.retry_after_on_new_key_deny();
        if self.buckets.contains_key(&bucket_key) {
            return Ok(());
        }
        let Some(max_keys) = self.cfg.max_keys else {
            return Ok(());
        };
        if self.buckets.len() < max_keys {
            return Ok(());
        }

        match self.cfg.overflow_policy {
            OverflowPolicy::DenyNewKey => {
                let deny = Deny { retry_after };
                self.observer.on_deny(DenyEvent {
                    route_key,
                    bucket_key,
                    cost,
                    now,
                    deny,
                    reason: DenyReason::KeyCardinalityBound,
                });
                Err(deny)
            }
            OverflowPolicy::EvictOldestKey => {
                // Evict oldest evictable (no outstanding reservations).
                let mut evict_idx = None;
                for (idx, k) in self.fifo.iter().enumerate() {
                    match self.buckets.get(k) {
                        Some(st) if st.has_outstanding() => {}
                        Some(_) | None => {
                            evict_idx = Some(idx);
                            break;
                        }
                    }
                }

                if let Some(idx) = evict_idx
                    && let Some(old) = self.fifo.remove(idx)
                {
                    self.buckets.remove(&old);
                    return Ok(());
                }

                // No evictable key found (all tracked keys have outstanding reservations).
                let deny = Deny { retry_after };
                self.observer.on_deny(DenyEvent {
                    route_key,
                    bucket_key,
                    cost,
                    now,
                    deny,
                    reason: DenyReason::KeyCardinalityBound,
                });
                Err(deny)
            }
        }
    }

    fn bucket_mut_or_insert(
        &mut self,
        route_key: Key,
        bucket_key: Key,
        now: Duration,
    ) -> Option<&mut BucketState> {
        if self.buckets.contains_key(&bucket_key) {
            return self.buckets.get_mut(&bucket_key);
        }

        if self
            .ensure_bucket_capacity(route_key, bucket_key, Cost::ONE, now)
            .is_err()
        {
            return None;
        }

        match self.buckets.entry(bucket_key) {
            Entry::Occupied(e) => Some(e.into_mut()),
            Entry::Vacant(v) => {
                self.fifo.push_back(bucket_key);
                Some(v.insert(BucketState::new(now, self.cfg.algorithm)))
            }
        }
    }

    fn effective_algorithm(&self, now: Duration) -> AlgorithmConfig {
        let Some(slow) = self.cfg.slow_start else {
            return self.cfg.algorithm;
        };

        match self.cfg.algorithm {
            AlgorithmConfig::FixedWindow {
                max_per_window,
                window,
            } => AlgorithmConfig::FixedWindow {
                max_per_window: slow_start::effective_max(
                    max_per_window,
                    slow,
                    self.started_at,
                    now,
                ),
                window,
            },
            AlgorithmConfig::TokenBucket {
                capacity,
                refill_per_sec,
            } => AlgorithmConfig::TokenBucket {
                capacity: slow_start::effective_u64(capacity, slow, self.started_at, now),
                refill_per_sec: slow_start::effective_u64(
                    refill_per_sec,
                    slow,
                    self.started_at,
                    now,
                ),
            },
        }
    }

    fn slow_start_pre_deny(
        &self,
        cost: Cost,
        now: Duration,
        effective_algo: AlgorithmConfig,
    ) -> Option<Deny> {
        let slow = self.cfg.slow_start?;

        match (self.cfg.algorithm, effective_algo) {
            (
                AlgorithmConfig::FixedWindow {
                    max_per_window: max_cfg,
                    window: _,
                },
                AlgorithmConfig::FixedWindow {
                    max_per_window: max_eff,
                    ..
                },
            ) => {
                if max_cfg == 0 {
                    return None;
                }
                if max_eff == 0 {
                    let ra = slow_start::retry_after_for_cost(
                        max_cfg,
                        slow,
                        self.started_at,
                        now,
                        cost.get(),
                    );
                    return Some(Deny { retry_after: ra });
                }
                if max_eff < cost.get() {
                    let ra = slow_start::retry_after_for_cost(
                        max_cfg,
                        slow,
                        self.started_at,
                        now,
                        cost.get(),
                    );
                    return Some(Deny { retry_after: ra });
                }
                None
            }
            (
                AlgorithmConfig::TokenBucket { capacity, .. },
                AlgorithmConfig::TokenBucket {
                    capacity: cap_eff, ..
                },
            ) => {
                let want = cost.get() as u64;
                if cap_eff >= want {
                    return None;
                }
                let ra = slow_start::retry_after_for_cost_u64(
                    capacity,
                    slow,
                    self.started_at,
                    now,
                    want,
                );
                Some(Deny { retry_after: ra })
            }
            _ => None,
        }
    }

    fn retry_after_on_new_key_deny(&self) -> Duration {
        match self.cfg.algorithm {
            AlgorithmConfig::FixedWindow { window, .. } => window,
            AlgorithmConfig::TokenBucket { .. } => Duration::from_secs(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::config::Config;
    use crate::feedback::{Feedback, FeedbackScope};
    use crate::slow_start::SlowStart;

    fn cfg(max: u32, window: Duration) -> Config {
        Config::fixed_window(max, window).unwrap()
    }

    #[derive(Debug)]
    struct CountingObserver {
        denies: Arc<AtomicU32>,
        feedbacks: Arc<AtomicU32>,
    }

    impl CountingObserver {
        fn new(denies: Arc<AtomicU32>, feedbacks: Arc<AtomicU32>) -> Self {
            Self { denies, feedbacks }
        }
    }

    impl LimiterObserver for CountingObserver {
        fn on_deny(&self, _ev: DenyEvent) {
            self.denies.fetch_add(1, Ordering::Relaxed);
        }

        fn on_feedback(&self, _ev: FeedbackEvent) {
            self.feedbacks.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[derive(Debug)]
    struct RecordingObserver {
        feedbacks: Arc<Mutex<Vec<FeedbackEvent>>>,
    }

    impl RecordingObserver {
        fn new(feedbacks: Arc<Mutex<Vec<FeedbackEvent>>>) -> Self {
            Self { feedbacks }
        }
    }

    impl LimiterObserver for RecordingObserver {
        fn on_feedback(&self, ev: FeedbackEvent) {
            self.feedbacks.lock().unwrap().push(ev);
        }
    }

    #[test]
    fn feedback_blocks_key_until_deadline_and_unknown_key_is_ok() {
        let mut rl = RateLimiter::new(cfg(10, Duration::from_secs(10)));
        rl.apply_feedback(
            123,
            &Feedback::new(Duration::from_secs(3), FeedbackScope::Key),
            Duration::from_secs(0),
        );

        let d = rl
            .try_acquire(123, Cost::ONE, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::from_secs(2));

        let _p = rl
            .try_acquire(123, Cost::ONE, Duration::from_secs(3))
            .unwrap();
    }

    #[test]
    fn feedback_remaining_zero_reset_after_extends_block() {
        let mut rl = RateLimiter::new(cfg(10, Duration::from_secs(10)));
        let fb = Feedback {
            retry_after: Duration::from_secs(1),
            scope: FeedbackScope::Key,
            remaining: Some(0),
            reset_after: Some(Duration::from_secs(5)),
            bucket_remap: None,
        };
        rl.apply_feedback(1, &fb, Duration::from_secs(0));

        let d = rl
            .try_acquire(1, Cost::ONE, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::from_secs(4));
    }

    #[test]
    fn feedback_bucket_remap_moves_future_acquires_to_new_bucket() {
        let mut rl = RateLimiter::new(cfg(1, Duration::from_secs(10)));
        let now = Duration::from_secs(0);

        let p1 = rl.try_acquire(77, Cost::ONE, now).unwrap();
        rl.commit(p1, Outcome::Confirmed, now);
        assert!(rl.try_acquire(77, Cost::ONE, now).is_err());

        let fb = Feedback {
            retry_after: Duration::ZERO,
            scope: FeedbackScope::Route,
            remaining: None,
            reset_after: None,
            bucket_remap: Some(999),
        };
        rl.apply_feedback(77, &fb, now);

        // Same route key now maps to a new bucket with fresh capacity.
        let p2 = rl.try_acquire(77, Cost::ONE, now).unwrap();
        assert_eq!(p2.key(), 999);
    }

    #[test]
    fn global_feedback_blocks_all_keys() {
        let mut rl = RateLimiter::new(cfg(10, Duration::from_secs(10)));
        rl.apply_feedback(
            1,
            &Feedback::new(Duration::from_secs(3), FeedbackScope::Global),
            Duration::from_secs(0),
        );
        let d = rl
            .try_acquire(2, Cost::ONE, Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::from_secs(2));
    }

    #[test]
    fn slow_start_scales_capacity_and_retry_after_is_actionable() {
        let cfg = cfg(10, Duration::from_secs(10))
            .with_slow_start(SlowStart::new(Duration::from_secs(10)));
        let mut rl = RateLimiter::new_at(cfg, Duration::from_secs(0));

        let d0 = rl
            .try_acquire(1, Cost::ONE, Duration::from_secs(0))
            .unwrap_err();
        assert_eq!(d0.retry_after, Duration::from_secs(1));

        let d_mid = rl
            .try_acquire(1, Cost::new(6).unwrap(), Duration::from_secs(5))
            .unwrap_err();
        assert_eq!(d_mid.retry_after, Duration::from_secs(1));

        let _p = rl
            .try_acquire(1, Cost::new(6).unwrap(), Duration::from_secs(6))
            .unwrap();
    }

    #[test]
    fn bounded_keys_eviction_fifo() {
        let cfg = Config::fixed_window_bounded(1, Duration::from_secs(10), Some(1))
            .unwrap()
            .with_overflow_policy(OverflowPolicy::EvictOldestKey);
        let mut rl = RateLimiter::new(cfg);
        let now = Duration::from_secs(0);

        let p1 = rl.try_acquire(1, Cost::ONE, now).unwrap();
        rl.commit(p1, Outcome::Confirmed, now);
        assert_eq!(rl.tracked_keys(), 1);

        let p2 = rl.try_acquire(2, Cost::ONE, now).unwrap();
        rl.commit(p2, Outcome::Confirmed, now);
        assert_eq!(rl.tracked_keys(), 1);

        // key=1 should have been evicted; acquiring it should evict key=2 and succeed.
        let _p3 = rl.try_acquire(1, Cost::ONE, now).unwrap();
        assert_eq!(rl.tracked_keys(), 1);
    }

    #[test]
    fn eviction_does_not_evict_outstanding_reservations() {
        let cfg = Config::fixed_window_bounded(10, Duration::from_secs(10), Some(1))
            .unwrap()
            .with_overflow_policy(OverflowPolicy::EvictOldestKey);
        let mut rl = RateLimiter::new(cfg);
        let now = Duration::from_secs(0);

        // Keep an outstanding reservation for key=1.
        let _p = rl.try_acquire(1, Cost::ONE, now).unwrap();
        let d = rl.try_acquire(2, Cost::ONE, now).unwrap_err();
        assert_eq!(d.retry_after, Duration::from_secs(10));
        assert_eq!(rl.tracked_keys(), 1);
    }

    #[test]
    fn eviction_skips_keys_with_outstanding_reservations() {
        let cfg = Config::fixed_window_bounded(10, Duration::from_secs(10), Some(2))
            .unwrap()
            .with_overflow_policy(OverflowPolicy::EvictOldestKey);
        let mut rl = RateLimiter::new(cfg);
        let now = Duration::from_secs(0);

        // Oldest key has an outstanding reservation and is not evictable.
        let _p1 = rl.try_acquire(1, Cost::ONE, now).unwrap();

        // Next key is evictable (no outstanding after commit).
        let p2 = rl.try_acquire(2, Cost::ONE, now).unwrap();
        rl.commit(p2, Outcome::Confirmed, now);

        // When inserting a third key, the limiter should evict key=2 (oldest evictable),
        // rather than denying due to key=1 being protected.
        let p3 = rl.try_acquire(3, Cost::ONE, now).unwrap();
        rl.commit(p3, Outcome::Confirmed, now);
    }

    #[test]
    fn observer_hooks_fire_once_for_deny_and_feedback() {
        let mut rl = RateLimiter::new(cfg(1, Duration::from_secs(10)));
        let denies = Arc::new(AtomicU32::new(0));
        let feedbacks = Arc::new(AtomicU32::new(0));
        rl.set_observer(Box::new(CountingObserver::new(
            Arc::clone(&denies),
            Arc::clone(&feedbacks),
        )));

        // Apply feedback should call observer.
        rl.apply_feedback(
            1,
            &Feedback::new(Duration::from_secs(2), FeedbackScope::Key),
            Duration::from_secs(0),
        );

        // Deny should call observer once.
        let _ = rl.try_acquire(1, Cost::ONE, Duration::from_secs(1));

        assert_eq!(feedbacks.load(Ordering::Relaxed), 1);
        assert_eq!(denies.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn token_bucket_deribit_non_matching_engine_defaults_example() {
        let mut rl = RateLimiter::new(Config::token_bucket(50_000, 10_000));
        let now = Duration::from_secs(0);
        let cost = Cost::new(500).unwrap();

        // Burst: 50_000 / 500 = 100 requests.
        for _ in 0..100 {
            let p = rl.try_acquire(1, cost, now).unwrap();
            rl.commit(p, Outcome::SentNoConfirm, now);
        }

        // Next should deny: deficit 500 credits, refill 10_000/sec => 50ms.
        let d = rl.try_acquire(1, cost, now).unwrap_err();
        assert_eq!(d.retry_after, Duration::from_millis(50));

        // After 50ms, one more should be allowed.
        let p = rl.try_acquire(1, cost, Duration::from_millis(50)).unwrap();
        rl.commit(p, Outcome::SentNoConfirm, Duration::from_millis(50));

        // Sustained: 10_000/sec => 20 req/sec. One second after the last spend (t=50ms), allow 20 requests.
        let t = Duration::from_millis(1050);
        for _ in 0..20 {
            let p = rl.try_acquire(1, cost, t).unwrap();
            rl.commit(p, Outcome::SentNoConfirm, t);
        }
        assert!(rl.try_acquire(1, cost, t).is_err());
    }

    #[test]
    fn token_bucket_deribit_subscribe_limits_example() {
        let mut rl = RateLimiter::new(Config::token_bucket(30_000, 10_000));
        let now = Duration::from_secs(0);
        let cost = Cost::new(3_000).unwrap();

        // Burst: 30_000 / 3_000 = 10.
        for _ in 0..10 {
            let p = rl.try_acquire(1, cost, now).unwrap();
            rl.commit(p, Outcome::SentNoConfirm, now);
        }

        let d = rl.try_acquire(1, cost, now).unwrap_err();
        assert_eq!(d.retry_after, Duration::from_millis(300));
    }

    #[test]
    fn token_bucket_update_semantics_capacity_and_refill() {
        let now0 = Duration::from_secs(0);
        let cost = Cost::new(5).unwrap();

        // Increasing capacity preserves deficit: credits increase by delta immediately.
        let mut rl = RateLimiter::new(Config::token_bucket(10, 0));
        let p = rl.try_acquire(1, cost, now0).unwrap();
        rl.commit(p, Outcome::SentNoConfirm, now0); // credits: 5
        rl.update_config(Config::token_bucket(20, 0), now0); // credits: 15
        for _ in 0..3 {
            let p = rl.try_acquire(1, cost, now0).unwrap();
            rl.commit(p, Outcome::SentNoConfirm, now0);
        }
        assert!(rl.try_acquire(1, cost, now0).is_err());

        // Decreasing capacity clamps credits.
        let mut rl = RateLimiter::new(Config::token_bucket(20, 0));
        let p = rl.try_acquire(1, cost, now0).unwrap();
        rl.commit(p, Outcome::SentNoConfirm, now0); // credits: 15
        rl.update_config(Config::token_bucket(10, 0), now0); // credits clamped to 10
        for _ in 0..2 {
            let p = rl.try_acquire(1, cost, now0).unwrap();
            rl.commit(p, Outcome::SentNoConfirm, now0);
        }
        assert!(rl.try_acquire(1, cost, now0).is_err());

        // Refill rate update affects retry_after deterministically.
        let mut rl = RateLimiter::new(Config::token_bucket(10, 10));
        let p = rl.try_acquire(1, Cost::new(10).unwrap(), now0).unwrap();
        rl.commit(p, Outcome::SentNoConfirm, now0);
        let d1 = rl.try_acquire(1, Cost::ONE, now0).unwrap_err();
        assert_eq!(d1.retry_after, Duration::from_millis(100));
        rl.update_config(Config::token_bucket(10, 100), now0);
        let d2 = rl.try_acquire(1, Cost::ONE, now0).unwrap_err();
        assert_eq!(d2.retry_after, Duration::from_millis(10));
    }

    #[test]
    fn token_bucket_respects_max_keys_when_set() {
        let cfg = Config::token_bucket(10, 10).with_max_keys(Some(1));
        let mut rl = RateLimiter::new(cfg);
        let now = Duration::from_secs(0);

        let p1 = rl.try_acquire(1, Cost::ONE, now).unwrap();
        rl.commit(p1, Outcome::Confirmed, now);

        let d = rl.try_acquire(2, Cost::ONE, now).unwrap_err();
        assert_eq!(d.retry_after, Duration::from_secs(1));
    }

    #[test]
    fn feedback_bucket_remap_event_reports_only_applied_remaps() {
        let cfg = Config::fixed_window_bounded(1, Duration::from_secs(10), Some(1)).unwrap();
        let mut rl = RateLimiter::new(cfg);

        let events = Arc::new(Mutex::new(Vec::<FeedbackEvent>::new()));
        rl.set_observer(Box::new(RecordingObserver::new(Arc::clone(&events))));

        // First remap fits within the route_map cap (max_keys=1).
        rl.apply_feedback(
            1,
            &Feedback {
                retry_after: Duration::ZERO,
                scope: FeedbackScope::Route,
                remaining: None,
                reset_after: None,
                bucket_remap: Some(10),
            },
            Duration::from_secs(0),
        );

        // Second remap is refused due to route_map being full.
        rl.apply_feedback(
            2,
            &Feedback {
                retry_after: Duration::ZERO,
                scope: FeedbackScope::Route,
                remaining: None,
                reset_after: None,
                bucket_remap: Some(20),
            },
            Duration::from_secs(0),
        );

        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 2);

        assert_eq!(evs[0].route_key, 1);
        assert_eq!(evs[0].bucket_key, 10);
        assert_eq!(evs[0].bucket_remap, Some(10));

        assert_eq!(evs[1].route_key, 2);
        assert_eq!(evs[1].bucket_key, 2);
        assert_eq!(evs[1].bucket_remap, None);
    }

    #[test]
    fn apply_feedback_duration_max_saturates_and_does_not_panic() {
        let mut rl = RateLimiter::new(cfg(10, Duration::from_secs(10)));
        rl.apply_feedback(
            1,
            &Feedback::new(Duration::MAX, FeedbackScope::Global),
            Duration::from_secs(1),
        );

        let d = rl
            .try_acquire(2, Cost::ONE, Duration::from_secs(2))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::MAX - Duration::from_secs(2));
    }

    #[test]
    fn outcome_global_feedback_duration_max_saturates_and_does_not_panic() {
        let mut rl = RateLimiter::new(cfg(10, Duration::from_secs(10)));
        let p = rl
            .try_acquire(1, Cost::ONE, Duration::from_secs(0))
            .unwrap();
        rl.commit(
            p,
            Outcome::RateLimitedFeedback {
                retry_after: Duration::MAX,
                scope: Scope::Global,
                bucket_hint: None,
            },
            Duration::from_secs(1),
        );

        let d = rl
            .try_acquire(2, Cost::ONE, Duration::from_secs(2))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::MAX - Duration::from_secs(2));
    }

    #[test]
    fn outcome_key_feedback_duration_max_saturates_and_does_not_panic() {
        let mut rl = RateLimiter::new(cfg(10, Duration::from_secs(10)));
        let p = rl
            .try_acquire(1, Cost::ONE, Duration::from_secs(0))
            .unwrap();
        rl.commit(
            p,
            Outcome::RateLimitedFeedback {
                retry_after: Duration::MAX,
                scope: Scope::Key,
                bucket_hint: None,
            },
            Duration::from_secs(1),
        );

        let d = rl
            .try_acquire(1, Cost::ONE, Duration::from_secs(2))
            .unwrap_err();
        assert_eq!(d.retry_after, Duration::MAX - Duration::from_secs(2));
    }
}
