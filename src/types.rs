use std::num::NonZeroU32;
use std::time::Duration;

/// Bucket key type.
///
/// Callers should pre-hash more complex keys to avoid copies and expensive hashing in the hot path.
pub type Key = u64;

/// A non-zero cost (weight) for a single operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cost(NonZeroU32);

impl Cost {
    /// Cost of 1.
    pub const ONE: Cost = Cost(NonZeroU32::MIN);

    /// Create a cost from a raw value. Returns `None` for `0`.
    pub const fn new(raw: u32) -> Option<Self> {
        // Stable const option: NonZeroU32::new is const.
        match NonZeroU32::new(raw) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Returns the cost as a `u32`.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl From<NonZeroU32> for Cost {
    fn from(value: NonZeroU32) -> Self {
        Self(value)
    }
}

/// A successful reservation of capacity.
///
/// This permit must be eventually finalized by calling `commit` or `refund`, which consume it.
#[derive(Debug, PartialEq, Eq)]
pub struct Permit {
    pub(crate) key: Key,
    pub(crate) cost: Cost,
    pub(crate) window_start: Duration,
    pub(crate) issued_at: Duration,
    pub(crate) deadline: Duration,
    pub(crate) meta: PermitMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermitMeta {
    FixedWindow { window_start: Duration },
    TokenBucket,
}

impl Permit {
    /// The key this permit was reserved against.
    pub fn key(&self) -> Key {
        self.key
    }

    /// The cost reserved by this permit.
    pub fn cost(&self) -> Cost {
        self.cost
    }

    /// When the permit was issued (as provided by the caller to `try_acquire`).
    pub fn issued_at(&self) -> Duration {
        self.issued_at
    }

    /// The deadline of the fixed window this permit belongs to.
    pub fn deadline(&self) -> Duration {
        self.deadline
    }
}

/// A denial response, including the recommended retry interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deny {
    /// How long the caller should wait before retrying.
    pub retry_after: Duration,
}

/// Protocol-agnostic outcome classification for accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Confirmation observed (HTTP response, WS ack, etc).
    Confirmed,
    /// Operation definitely did not go out; safe to refund.
    NotSent,
    /// Operation likely went out but no confirmation observed; do not refund by default.
    SentNoConfirm,
    /// Server explicitly reported a rate limit event.
    RateLimitedFeedback {
        /// Suggested retry delay.
        retry_after: Duration,
        /// Scope of the limit (global or key-local).
        scope: Scope,
        /// Optional hint for a bucket id; used in later sprints for dynamic remaps.
        bucket_hint: Option<Key>,
    },
}

/// Scope of a server-provided rate limit signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Applies globally (across all keys).
    Global,
    /// Applies to the current key/bucket.
    Key,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_new_rejects_zero() {
        assert!(Cost::new(0).is_none());
        assert_eq!(Cost::new(1).unwrap().get(), 1);
    }
}
