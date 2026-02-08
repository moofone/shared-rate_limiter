//! Integration tests (public API only) for Discord-like rate limit feedback behavior.
//!
//! Reference: https://discord.com/developers/docs/topics/rate-limits

use std::time::Duration;

use shared_rate_limiter::{Config, Cost, Feedback, FeedbackScope, RateLimiter};

#[test]
fn discord_route_bucket_remaining_zero_blocks_until_reset_after() {
    // Discord commonly provides headers like:
    // - X-RateLimit-Remaining: 0
    // - X-RateLimit-Reset-After: <seconds>
    // Model this as key-scoped feedback with remaining=0 and reset_after.
    let mut rl = RateLimiter::new(Config::fixed_window(10, Duration::from_secs(10)).unwrap());

    let fb = Feedback {
        retry_after: Duration::ZERO,
        scope: FeedbackScope::Key,
        remaining: Some(0),
        reset_after: Some(Duration::from_secs(2)),
        bucket_remap: None,
    };
    rl.apply_feedback(9, &fb, Duration::from_secs(0));

    let d = rl
        .try_acquire(9, Cost::ONE, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(1));

    let _p = rl
        .try_acquire(9, Cost::ONE, Duration::from_secs(2))
        .unwrap();
}

#[test]
fn discord_global_429_blocks_all_keys() {
    // Discord can enforce a global rate limit; model as global feedback.
    let mut rl = RateLimiter::new(Config::fixed_window(10, Duration::from_secs(10)).unwrap());

    rl.apply_feedback(
        0,
        &Feedback::new(Duration::from_secs(3), FeedbackScope::Global),
        Duration::from_secs(0),
    );

    let d = rl
        .try_acquire(123, Cost::ONE, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(2));
}
