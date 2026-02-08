//! Integration tests (public API only) for Deribit-like rate limits.
//!
//! Reference: https://support.deribit.com/hc/en-us/articles/25944617523357-Rate-Limits

use std::time::Duration;

use shared_rate_limiter::{Config, Cost, Outcome, RateLimiter};

#[test]
fn deribit_non_matching_engine_credit_pool_defaults() {
    // Deribit docs describe a credit pool model. The commonly-cited defaults are:
    // - pool 50_000 credits
    // - refill 10_000 credits/sec
    // - default (non-matching-engine) request cost 500 credits
    let mut rl = RateLimiter::new(Config::token_bucket(50_000, 10_000));
    let now = Duration::from_secs(0);

    for _ in 0..100 {
        let p = rl.try_acquire(1, Cost::new(500).unwrap(), now).unwrap();
        rl.commit(p, Outcome::SentNoConfirm, now);
    }

    // Pool exhausted; next request should be denied until enough credits refill.
    let d = rl.try_acquire(1, Cost::new(500).unwrap(), now).unwrap_err();
    assert_eq!(d.retry_after, Duration::from_millis(50));

    // After 50ms, 500 credits have refilled; allow one more request.
    let p = rl
        .try_acquire(1, Cost::new(500).unwrap(), Duration::from_millis(50))
        .unwrap();
    rl.commit(p, Outcome::SentNoConfirm, Duration::from_millis(50));
}

#[test]
fn deribit_subscribe_credit_pool_limits() {
    // Subscribe cost and pool are different from defaults (per docs/examples).
    // - pool 30_000 credits
    // - refill 10_000 credits/sec
    // - subscribe cost 3_000 credits
    let mut rl = RateLimiter::new(Config::token_bucket(30_000, 10_000));
    let now = Duration::from_secs(0);

    for _ in 0..10 {
        let p = rl.try_acquire(2, Cost::new(3_000).unwrap(), now).unwrap();
        rl.commit(p, Outcome::SentNoConfirm, now);
    }

    let d = rl
        .try_acquire(2, Cost::new(3_000).unwrap(), now)
        .unwrap_err();
    assert_eq!(d.retry_after, Duration::from_millis(300));

    let p = rl
        .try_acquire(2, Cost::new(3_000).unwrap(), Duration::from_millis(300))
        .unwrap();
    rl.commit(p, Outcome::SentNoConfirm, Duration::from_millis(300));
}
