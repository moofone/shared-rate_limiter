#![cfg(feature = "kameo")]

use std::time::Duration;

use shared_rate_limiter::kameo_actor::{RateLimiterActor, SingletonError};
use shared_rate_limiter::{Config, RateLimiter};

#[tokio::test]
async fn singleton_enforces_name_mismatch() {
    let cfg = Config::fixed_window(1, Duration::from_secs(1)).unwrap();
    let _a1 =
        RateLimiterActor::spawn_singleton("rl_singleton_test".to_string(), RateLimiter::new(cfg))
            .await
            .unwrap();

    let err = RateLimiterActor::spawn_singleton(
        "rl_singleton_test_other".to_string(),
        RateLimiter::new(Config::fixed_window(1, Duration::from_secs(1)).unwrap()),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SingletonError::NameMismatch { .. }));
}
