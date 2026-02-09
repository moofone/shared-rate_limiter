use std::time::Duration;

use crate::slow_start::SlowStart;

/// Algorithm configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlgorithmConfig {
    /// Fixed-window limiter.
    FixedWindow {
        /// Maximum capacity per window.
        max_per_window: u32,
        /// Fixed window size.
        window: Duration,
    },
    /// Token-bucket / credit-pool limiter (Deribit-like).
    ///
    /// - `capacity`: maximum credits in the pool.
    /// - `refill_per_sec`: credits refilled per second.
    TokenBucket {
        /// Maximum credits in the pool.
        capacity: u64,
        /// Credits refilled per second.
        refill_per_sec: u64,
    },
}

/// Rate limiter configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) algorithm: AlgorithmConfig,
    pub(crate) max_keys: Option<usize>,
    pub(crate) overflow_policy: OverflowPolicy,
    pub(crate) slow_start: Option<SlowStart>,
}

/// Errors returned by [`Config`] constructors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// A fixed-window limiter must have a non-zero window.
    ZeroWindow,
}

/// Behavior when the key cardinality bound is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Deny creating new keys once the map is full.
    DenyNewKey,
    /// Evict the oldest key (FIFO) to make room for a new key.
    EvictOldestKey,
}

impl Config {
    /// Fixed-window limiter configuration.
    ///
    /// `max_keys` defaults to `None` (unbounded distinct keys).
    pub fn fixed_window(max_per_window: u32, window: Duration) -> Result<Self, ConfigError> {
        Self::fixed_window_bounded(max_per_window, window, None)
    }

    /// Fixed-window limiter configuration with an optional max distinct key bound.
    ///
    /// If `max_keys` is `Some(n)`, acquiring a previously unseen key when `n` keys already exist
    /// will be denied.
    pub fn fixed_window_bounded(
        max_per_window: u32,
        window: Duration,
        max_keys: Option<usize>,
    ) -> Result<Self, ConfigError> {
        if window.is_zero() {
            return Err(ConfigError::ZeroWindow);
        }
        Ok(Self {
            algorithm: AlgorithmConfig::FixedWindow {
                max_per_window,
                window,
            },
            max_keys,
            overflow_policy: OverflowPolicy::DenyNewKey,
            slow_start: None,
        })
    }

    /// Token-bucket / credit-pool limiter configuration.
    ///
    /// `max_keys` defaults to `None` (unbounded distinct keys). Use [`Config::with_max_keys`] to
    /// apply a bound.
    pub fn token_bucket(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            algorithm: AlgorithmConfig::TokenBucket {
                capacity,
                refill_per_sec,
            },
            max_keys: None,
            overflow_policy: OverflowPolicy::DenyNewKey,
            slow_start: None,
        }
    }

    /// Set the maximum number of distinct tracked keys.
    ///
    /// When set, acquiring a previously unseen key past this bound will be denied (or evict an
    /// existing key) depending on the configured [`OverflowPolicy`].
    pub fn with_max_keys(mut self, max_keys: Option<usize>) -> Self {
        self.max_keys = max_keys;
        self
    }

    /// Enable slow-start ramping.
    pub fn with_slow_start(mut self, slow_start: SlowStart) -> Self {
        self.slow_start = Some(slow_start);
        self
    }

    /// Set the key overflow policy.
    pub fn with_overflow_policy(mut self, policy: OverflowPolicy) -> Self {
        self.overflow_policy = policy;
        self
    }

    /// Returns the fixed window size.
    ///
    /// This is only meaningful for [`AlgorithmConfig::FixedWindow`]. For token buckets it returns `Duration::ZERO`.
    pub fn window(&self) -> Duration {
        match self.algorithm {
            AlgorithmConfig::FixedWindow { window, .. } => window,
            AlgorithmConfig::TokenBucket { .. } => Duration::ZERO,
        }
    }

    /// Returns the max capacity per window.
    ///
    /// This is only meaningful for [`AlgorithmConfig::FixedWindow`]. For token buckets it returns `0`.
    pub fn max_per_window(&self) -> u32 {
        match self.algorithm {
            AlgorithmConfig::FixedWindow { max_per_window, .. } => max_per_window,
            AlgorithmConfig::TokenBucket { .. } => 0,
        }
    }

    /// Returns the configured algorithm.
    pub fn algorithm(&self) -> AlgorithmConfig {
        self.algorithm
    }

    /// Optional bound on distinct tracked keys.
    pub fn max_keys(&self) -> Option<usize> {
        self.max_keys
    }

    /// Overflow behavior when `max_keys` is exceeded.
    pub fn overflow_policy(&self) -> OverflowPolicy {
        self.overflow_policy
    }

    /// Optional slow-start configuration.
    pub fn slow_start(&self) -> Option<SlowStart> {
        self.slow_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_window_rejects_zero_window() {
        assert_eq!(
            Config::fixed_window(1, Duration::ZERO).unwrap_err(),
            ConfigError::ZeroWindow
        );
    }
}
