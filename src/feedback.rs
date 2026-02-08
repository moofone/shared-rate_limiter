use std::time::Duration;

use crate::types::Key;

/// Scope of a server-provided rate limit signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackScope {
    /// Applies globally across all keys.
    Global,
    /// Applies to the current bucket key.
    Key,
    /// Applies to a route key (which may be mapped to a bucket key).
    Route,
}

/// Protocol-agnostic representation of a server rate-limit feedback signal.
///
/// Protocol adapters should implement this trait for their response/error types.
pub trait RateLimitFeedback {
    /// Suggested backoff delay.
    fn retry_after(&self) -> Duration;

    /// Scope of the limit.
    fn scope(&self) -> FeedbackScope;

    /// Optional "remaining" signal (Discord-like). Semantics are adapter-defined.
    ///
    /// In this core crate, `remaining == Some(0)` combined with `reset_after` will be used
    /// to block until reset.
    fn remaining(&self) -> Option<u32> {
        None
    }

    /// Optional reset delay (Discord-like). Semantics are adapter-defined.
    fn reset_after(&self) -> Option<Duration> {
        None
    }

    /// Optional remap from a route key to a server-assigned bucket key.
    fn bucket_remap(&self) -> Option<Key> {
        None
    }
}

/// A simple feedback value type for callers that don't want to define their own implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feedback {
    /// Suggested backoff delay.
    pub retry_after: Duration,
    /// Scope of the limit.
    pub scope: FeedbackScope,
    /// Optional remaining count.
    pub remaining: Option<u32>,
    /// Optional reset delay.
    pub reset_after: Option<Duration>,
    /// Optional remap target.
    pub bucket_remap: Option<Key>,
}

impl Feedback {
    /// Construct a minimal feedback with `retry_after` and `scope`.
    pub fn new(retry_after: Duration, scope: FeedbackScope) -> Self {
        Self {
            retry_after,
            scope,
            remaining: None,
            reset_after: None,
            bucket_remap: None,
        }
    }
}

impl RateLimitFeedback for Feedback {
    fn retry_after(&self) -> Duration {
        self.retry_after
    }

    fn scope(&self) -> FeedbackScope {
        self.scope
    }

    fn remaining(&self) -> Option<u32> {
        self.remaining
    }

    fn reset_after(&self) -> Option<Duration> {
        self.reset_after
    }

    fn bucket_remap(&self) -> Option<Key> {
        self.bucket_remap
    }
}
