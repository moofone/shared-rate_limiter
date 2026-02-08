//! Integration tests (public API only) for Bybit rate limit shapes.
//!
//! Reference: https://bybit-exchange.github.io/docs/v5/rate-limit

use std::time::Duration;

use shared_rate_limiter::{Config, Cost, Feedback, FeedbackScope, Outcome, RateLimiter};

#[test]
fn bybit_http_ip_rate_limit_600_per_5s() {
    // Bybit docs (V5) state a per-IP limit of 600 requests per 5 seconds for the HTTP API.
    let mut rl = RateLimiter::new(Config::fixed_window(600, Duration::from_secs(5)).unwrap());
    let now = Duration::from_secs(0);

    for _ in 0..600 {
        let p = rl.try_acquire(1, Cost::ONE, now).unwrap();
        rl.commit(p, Outcome::Confirmed, now);
    }

    let d = rl.try_acquire(1, Cost::ONE, now).unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(5));

    // New window should allow again once 5 seconds have elapsed.
    let p = rl
        .try_acquire(1, Cost::ONE, Duration::from_secs(5))
        .unwrap();
    rl.commit(p, Outcome::Confirmed, Duration::from_secs(5));
}

#[test]
fn bybit_ws_ip_connect_limit_500_per_5m() {
    // Bybit docs (WebSocket) state: 500 connections per 5 minutes per IP.
    // Model "connection attempts" as the unit being limited.
    let mut rl = RateLimiter::new(Config::fixed_window(500, Duration::from_secs(300)).unwrap());
    let now = Duration::from_secs(0);

    for _ in 0..500 {
        let p = rl.try_acquire(2, Cost::ONE, now).unwrap();
        rl.commit(p, Outcome::Confirmed, now);
    }

    let d = rl.try_acquire(2, Cost::ONE, now).unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(300));

    let p = rl
        .try_acquire(2, Cost::ONE, Duration::from_secs(300))
        .unwrap();
    rl.commit(p, Outcome::Confirmed, Duration::from_secs(300));
}

#[test]
fn bybit_http_ban_feedback_blocks_for_10_minutes() {
    // Bybit docs state that if you hit "403, access too frequent" you should wait at least 10
    // minutes; model that as a global feedback backoff.
    let mut rl = RateLimiter::new(Config::fixed_window(10, Duration::from_secs(1)).unwrap());

    rl.apply_feedback(
        0,
        &Feedback::new(Duration::from_secs(600), FeedbackScope::Global),
        Duration::from_secs(0),
    );

    let d = rl
        .try_acquire(123, Cost::ONE, Duration::from_secs(599))
        .unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(1));
}
