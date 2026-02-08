use std::time::Duration;

use shared_rate_limiter::{Config, Cost, Feedback, FeedbackScope, RateLimiter};
use shared_rate_limiter::{Outcome, Scope};

#[test]
fn feedback_global_blocks_all_keys_and_does_not_shorten_existing_backoff() {
    let mut rl = RateLimiter::new(Config::fixed_window(10, Duration::from_secs(10)).unwrap());

    rl.apply_feedback(
        1,
        &Feedback::new(Duration::from_secs(3), FeedbackScope::Global),
        Duration::from_secs(0),
    );

    // A different key should also be blocked due to global scope.
    let d = rl
        .try_acquire(2, Cost::ONE, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(2));

    // A smaller subsequent backoff must not reduce the existing global block.
    rl.apply_feedback(
        1,
        &Feedback::new(Duration::from_secs(1), FeedbackScope::Global),
        Duration::from_secs(0),
    );
    let d2 = rl
        .try_acquire(2, Cost::ONE, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(d2.retry_after, Duration::from_secs(2));
}

#[test]
fn feedback_key_scope_uses_reset_after_when_remaining_is_zero() {
    let mut rl = RateLimiter::new(Config::fixed_window(10, Duration::from_secs(10)).unwrap());

    let fb = Feedback {
        retry_after: Duration::from_secs(1),
        scope: FeedbackScope::Key,
        remaining: Some(0),
        reset_after: Some(Duration::from_secs(5)),
        bucket_remap: None,
    };
    rl.apply_feedback(7, &fb, Duration::from_secs(0));

    let d = rl
        .try_acquire(7, Cost::ONE, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(4));
}

#[test]
fn outcome_rate_limited_feedback_global_blocks_all_keys() {
    let mut rl = RateLimiter::new(Config::fixed_window(10, Duration::from_secs(10)).unwrap());

    // Acquire/commit isn't required for global scope in this crate, but the Outcome path is used by
    // adapters that finalize a Permit with a "server said 429" classification.
    let p = rl
        .try_acquire(1, Cost::ONE, Duration::from_secs(0))
        .unwrap();
    rl.commit(
        p,
        Outcome::RateLimitedFeedback {
            retry_after: Duration::from_secs(3),
            scope: Scope::Global,
            bucket_hint: None,
        },
        Duration::from_secs(0),
    );

    let d = rl
        .try_acquire(2, Cost::ONE, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(d.retry_after, Duration::from_secs(2));
}
