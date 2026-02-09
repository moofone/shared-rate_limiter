//! Protocol-agnostic core rate limiter.
//!
//! This crate provides a fast, deterministic, **synchronous** (non-async) limiter with
//! two-phase `Permit` semantics:
//! - `try_acquire` reserves capacity and returns a `Permit` on success.
//! - The caller must later `commit` or `refund` the `Permit` (both consume it).
//!
//! The first algorithm implemented is a per-key **fixed window** limiter.
//!
//! # Representing HTTP 429 / Retry-After
//! Use [`Feedback`] + [`RateLimiter::apply_feedback`] to apply server backoff signals without
//! requiring a `Permit`:
//! ```rust
//! use std::time::Duration;
//!
//! use shared_rate_limiter::{Config, Cost, Feedback, FeedbackScope, RateLimiter};
//!
//! let cfg = Config::fixed_window(10, Duration::from_secs(10)).unwrap();
//! let mut limiter = RateLimiter::new(cfg);
//! let now = Duration::from_secs(0);
//!
//! // HTTP 429 with Retry-After: 2s (global or per-route semantics are adapter-defined).
//! limiter.apply_feedback(42, &Feedback::new(Duration::from_secs(2), FeedbackScope::Key), now);
//! assert!(limiter.try_acquire(42, Cost::ONE, Duration::from_secs(1)).is_err());
//! ```
//!
//! # Representing Discord headers (conceptual)
//! A Discord adapter can implement [`RateLimitFeedback`] to surface:
//! - `remaining` + `reset_after` (to block until reset when remaining is 0)
//! - optional `bucket_remap` (route key -> bucket id)
//!
//! # In-flight deduplication
//! For kameo delegated-reply style integrations, use [`InFlightTable`] to deduplicate duplicate request ids
//! while an initial attempt is in-flight, and [`LimiterInFlight`] to carry permits through completion/expiry.
//!
//! # Token Bucket (Deribit-like Credit Pool)
//! Use [`Config::token_bucket`] to model a bursty credit pool with a sustained refill rate:
//! ```rust
//! use std::time::Duration;
//!
//! use shared_rate_limiter::{Config, Cost, Outcome, RateLimiter};
//!
//! // 50_000 credits pool, refills 10_000 credits/sec.
//! let cfg = Config::token_bucket(50_000, 10_000);
//! let mut limiter = RateLimiter::new(cfg);
//! let now = Duration::from_secs(0);
//!
//! // Each request costs 500 credits.
//! let p = limiter.try_acquire(42, Cost::new(500).unwrap(), now).unwrap();
//! limiter.commit(p, Outcome::SentNoConfirm, now);
//! ```
//!
//! ## Runtime snapshot updates
//! To apply updated limits deterministically at runtime (e.g., hourly Deribit account summary),
//! use [`RateLimiter::update_config`]:
//! ```rust
//! use std::time::Duration;
//! use shared_rate_limiter::{Config, RateLimiter};
//!
//! let mut limiter = RateLimiter::new(Config::token_bucket(10, 10));
//! limiter
//!     .update_config(Config::token_bucket(20, 10), Duration::from_secs(0))
//!     .unwrap();
//! ```
//!
//! # Example
//! ```rust
//! use std::time::Duration;
//!
//! use shared_rate_limiter::{Config, Cost, Outcome, RateLimiter};
//!
//! let cfg = Config::fixed_window(3, Duration::from_secs(10)).unwrap();
//! let mut limiter = RateLimiter::new(cfg);
//! let now = Duration::from_secs(0);
//!
//! let p = limiter.try_acquire(42, Cost::ONE, now).unwrap();
//! limiter.commit(p, Outcome::Confirmed, now);
//! ```
//!
//! # Linear permit semantics
//! Permits are **linear**: `commit` and `refund` take `Permit` by value, so it cannot be used twice.
//! The following does not compile:
//! ```compile_fail
//! use std::time::Duration;
//! use shared_rate_limiter::{Config, Cost, Outcome, RateLimiter};
//!
//! let cfg = Config::fixed_window(1, Duration::from_secs(1)).unwrap();
//! let mut limiter = RateLimiter::new(cfg);
//! let now = Duration::from_secs(0);
//! let p = limiter.try_acquire(1, Cost::ONE, now).unwrap();
//! limiter.refund(p, now);
//! limiter.commit(p, Outcome::Confirmed, now); // moved
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Optional clock helpers (the core API takes `now` explicitly).
pub mod clock;
/// Limiter configuration types.
pub mod config;
/// Protocol-agnostic representation of server rate-limit feedback.
pub mod feedback;
/// In-flight request deduplication table with deadline expiry.
pub mod in_flight;
/// Observer hook surface for deny/feedback events.
pub mod observer;
/// Slow-start ramp configuration and helpers.
pub mod slow_start;
/// Token-bucket (credit pool) limiter implementation (internal).
mod token_bucket;
/// Public types used by the core API.
pub mod types;

mod bucket;
mod hash;
mod limiter;

pub use config::{AlgorithmConfig, Config, ConfigError, OverflowPolicy};
pub use feedback::{Feedback, FeedbackScope, RateLimitFeedback};
pub use limiter::RateLimiter;
pub use limiter::{UpdateConfigError, UpdateConfigErrorKind};
pub use observer::{DenyEvent, DenyReason, FeedbackEvent, LimiterObserver};
pub use slow_start::SlowStart;
pub use types::{Cost, Deny, Key, Outcome, Permit, Scope};

pub use in_flight::{
    BeginResult, InFlightTable, LimiterInFlight, LimiterInFlightExpiryPolicy, PayloadMismatch,
};

/// Optional kameo actor wrapper (feature-gated).
#[cfg(feature = "kameo")]
pub mod kameo_actor;
