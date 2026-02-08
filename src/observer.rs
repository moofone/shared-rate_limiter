use std::time::Duration;

use crate::feedback::FeedbackScope;
use crate::types::{Cost, Deny, Key};

/// Reason for a deny decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The limiter is within slow-start ramp and effective capacity is too low.
    SlowStart,
    /// Key creation is denied due to configured cardinality bound.
    KeyCardinalityBound,
    /// The key (or global scope) is blocked until a deadline due to feedback.
    FeedbackBackoff,
    /// Fixed-window capacity is exhausted for the current window.
    WindowExhausted,
    /// The configured steady-state maximum is zero.
    Disabled,
}

/// Observer payload for deny events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenyEvent {
    /// The route key passed to the limiter.
    pub route_key: Key,
    /// The resolved bucket key used for accounting.
    pub bucket_key: Key,
    /// Cost requested.
    pub cost: Cost,
    /// Timestamp associated with the decision.
    pub now: Duration,
    /// Deny response (includes retry-after).
    pub deny: Deny,
    /// Reason classification.
    pub reason: DenyReason,
}

/// Observer payload for feedback application events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackEvent {
    /// The route key passed to the limiter.
    pub route_key: Key,
    /// The resolved bucket key used for accounting (after remap, if any).
    pub bucket_key: Key,
    /// Timestamp associated with the application.
    pub now: Duration,
    /// Backoff delay.
    pub retry_after: Duration,
    /// Feedback scope.
    pub scope: FeedbackScope,
    /// Optional remap target.
    pub bucket_remap: Option<Key>,
}

/// Observer hook surface for deny/feedback events.
///
/// This crate does not force a logging backend; applications may connect `tracing` or other
/// logging by implementing this trait.
pub trait LimiterObserver: Send + Sync + 'static {
    /// Called when a request is denied.
    fn on_deny(&self, _ev: DenyEvent) {}

    /// Called when feedback is applied.
    fn on_feedback(&self, _ev: FeedbackEvent) {}
}

/// No-op observer (default).
#[derive(Debug, Default)]
pub struct NoopObserver;

impl LimiterObserver for NoopObserver {}
