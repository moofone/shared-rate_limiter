use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use heed::types::{Bytes, Str};
use heed::{Database, Env, EnvOpenOptions};
use rkyv::{Archive, Deserialize, Serialize};

const DEFAULT_DB_NAME: &str = "open_gate";
const DEFAULT_RECORD_KEY: &str = "global_open_gate";

/// Configuration for the durable open gate.
#[derive(Debug, Clone)]
pub struct DurableOpenGateConfig {
    /// Dedup window width.
    pub window: Duration,
    /// LMDB map size in bytes.
    pub lmdb_map_size_bytes: usize,
    /// LMDB database name.
    pub db_name: &'static str,
    /// Logical key used for the global gate record.
    pub record_key: &'static str,
    /// When true, persistence failures deny opens (fail closed).
    pub fail_closed: bool,
}

impl Default for DurableOpenGateConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(30 * 60),
            lmdb_map_size_bytes: 32 * 1024 * 1024,
            db_name: DEFAULT_DB_NAME,
            record_key: DEFAULT_RECORD_KEY,
            fail_closed: true,
        }
    }
}

/// Decision returned by [`DurableOpenGate::try_acquire_open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenGateDecision {
    /// The open was granted and lock is now active until `lock_until`.
    Allowed {
        /// Timestamp when the lock expires.
        lock_until: Duration,
    },
    /// The open was blocked because lock is active until `lock_until`.
    Blocked {
        /// Timestamp when the lock expires.
        lock_until: Duration,
    },
}

/// In-memory counters for gate behavior and persistence load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenGateStats {
    /// Total acquire attempts.
    pub attempts_total: u64,
    /// Total granted attempts.
    pub allowed_total: u64,
    /// Total blocked attempts.
    pub blocked_total: u64,
    /// Total persistence reads.
    pub persist_reads_total: u64,
    /// Total persistence writes.
    pub persist_writes_total: u64,
    /// Total persistence failures.
    pub persist_failures_total: u64,
}

/// Durable open gate errors.
#[derive(Debug)]
pub enum DurableOpenGateError {
    /// Invalid configuration.
    InvalidConfig(String),
    /// Filesystem I/O failure.
    Io(std::io::Error),
    /// LMDB/heed failure.
    Heed(heed::Error),
    /// rkyv encode failure.
    Encode(String),
    /// rkyv decode failure.
    Decode(String),
}

impl fmt::Display for DurableOpenGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid durable open gate config: {msg}"),
            Self::Io(err) => write!(f, "durable open gate I/O error: {err}"),
            Self::Heed(err) => write!(f, "durable open gate LMDB error: {err}"),
            Self::Encode(err) => write!(f, "durable open gate encode error: {err}"),
            Self::Decode(err) => write!(f, "durable open gate decode error: {err}"),
        }
    }
}

impl std::error::Error for DurableOpenGateError {}

impl From<std::io::Error> for DurableOpenGateError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<heed::Error> for DurableOpenGateError {
    fn from(value: heed::Error) -> Self {
        Self::Heed(value)
    }
}

#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
struct OpenGateRecordV1 {
    version: u16,
    lock_until_ms: u64,
    updated_at_ms: u64,
    window_ms: u64,
}

/// Durable LMDB-backed gate that enforces at-most-one open grant per configured window.
pub struct DurableOpenGate {
    env: Env,
    db: Database<Str, Bytes>,
    cfg: DurableOpenGateConfig,
    cached_lock_until_ms: u64,
    stats: OpenGateStats,
}

impl fmt::Debug for DurableOpenGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DurableOpenGate")
            .field("cfg", &self.cfg)
            .field("cached_lock_until_ms", &self.cached_lock_until_ms)
            .field("stats", &self.stats)
            .finish()
    }
}

impl DurableOpenGate {
    /// Opens or creates a durable open gate rooted at `path`.
    pub fn open(
        path: impl AsRef<Path>,
        cfg: DurableOpenGateConfig,
    ) -> Result<Self, DurableOpenGateError> {
        if cfg.window.is_zero() {
            return Err(DurableOpenGateError::InvalidConfig(
                "window must be non-zero".to_string(),
            ));
        }
        if cfg.lmdb_map_size_bytes < 1024 * 1024 {
            return Err(DurableOpenGateError::InvalidConfig(
                "lmdb_map_size_bytes must be at least 1 MiB".to_string(),
            ));
        }

        fs::create_dir_all(path.as_ref())?;
        let env = unsafe {
            // SAFETY: caller provides a dedicated LMDB directory path, and this crate keeps a
            // single `Env` handle per path in-process.
            EnvOpenOptions::new()
                .map_size(cfg.lmdb_map_size_bytes)
                .max_dbs(4)
                .open(path.as_ref())?
        };

        let mut wtxn = env.write_txn()?;
        let db = env.create_database::<Str, Bytes>(&mut wtxn, Some(cfg.db_name))?;
        wtxn.commit()?;

        let mut gate = Self {
            env,
            db,
            cfg,
            cached_lock_until_ms: 0,
            stats: OpenGateStats::default(),
        };
        match gate.load_lock_until_ms() {
            Ok(value) => gate.cached_lock_until_ms = value,
            Err(err) => {
                gate.stats.persist_failures_total =
                    gate.stats.persist_failures_total.saturating_add(1);
                if gate.cfg.fail_closed {
                    return Err(err);
                }
            }
        }
        Ok(gate)
    }

    /// Returns a copy of current counters.
    pub fn stats(&self) -> OpenGateStats {
        self.stats
    }

    /// Attempts to acquire the global open gate.
    pub fn try_acquire_open(
        &mut self,
        now: Duration,
    ) -> Result<OpenGateDecision, DurableOpenGateError> {
        self.stats.attempts_total = self.stats.attempts_total.saturating_add(1);
        let now_ms = duration_to_millis(now);

        // Hot-path fast deny: blocked attempts inside cache window never touch LMDB.
        if now_ms < self.cached_lock_until_ms {
            self.stats.blocked_total = self.stats.blocked_total.saturating_add(1);
            return Ok(OpenGateDecision::Blocked {
                lock_until: millis_to_duration(self.cached_lock_until_ms),
            });
        }

        let mut wtxn = self.env.write_txn()?;
        let persisted_lock_until = match self.read_lock_until_ms_rw(&wtxn) {
            Ok(value) => value,
            Err(err) => {
                drop(wtxn);
                return self.persist_error_decision(err, now_ms);
            }
        };
        let effective_lock_until = self.cached_lock_until_ms.max(persisted_lock_until);
        if now_ms < effective_lock_until {
            drop(wtxn);
            self.cached_lock_until_ms = effective_lock_until;
            self.stats.persist_reads_total = self.stats.persist_reads_total.saturating_add(1);
            self.stats.blocked_total = self.stats.blocked_total.saturating_add(1);
            return Ok(OpenGateDecision::Blocked {
                lock_until: millis_to_duration(effective_lock_until),
            });
        }

        let lock_until_ms = now_ms.saturating_add(duration_to_millis(self.cfg.window));
        let record = OpenGateRecordV1 {
            version: 1,
            lock_until_ms,
            updated_at_ms: now_ms,
            window_ms: duration_to_millis(self.cfg.window),
        };
        let bytes = match rkyv::to_bytes::<rkyv::rancor::Error>(&record) {
            Ok(value) => value.to_vec(),
            Err(err) => {
                drop(wtxn);
                return self
                    .persist_error_decision(DurableOpenGateError::Encode(err.to_string()), now_ms);
            }
        };
        if let Err(err) = self
            .db
            .put(&mut wtxn, self.cfg.record_key, bytes.as_slice())
        {
            drop(wtxn);
            return self.persist_error_decision(err.into(), now_ms);
        }
        if let Err(err) = wtxn.commit() {
            return self.persist_error_decision(err.into(), now_ms);
        }

        self.cached_lock_until_ms = lock_until_ms;
        self.stats.persist_reads_total = self.stats.persist_reads_total.saturating_add(1);
        self.stats.persist_writes_total = self.stats.persist_writes_total.saturating_add(1);
        self.stats.allowed_total = self.stats.allowed_total.saturating_add(1);

        Ok(OpenGateDecision::Allowed {
            lock_until: millis_to_duration(lock_until_ms),
        })
    }

    fn persist_error_decision(
        &mut self,
        err: DurableOpenGateError,
        now_ms: u64,
    ) -> Result<OpenGateDecision, DurableOpenGateError> {
        self.stats.persist_failures_total = self.stats.persist_failures_total.saturating_add(1);
        if self.cfg.fail_closed {
            return Err(err);
        }

        let lock_until_ms = now_ms.saturating_add(duration_to_millis(self.cfg.window));
        self.cached_lock_until_ms = lock_until_ms;
        self.stats.allowed_total = self.stats.allowed_total.saturating_add(1);
        Ok(OpenGateDecision::Allowed {
            lock_until: millis_to_duration(lock_until_ms),
        })
    }

    fn load_lock_until_ms(&mut self) -> Result<u64, DurableOpenGateError> {
        let rtxn = self.env.read_txn()?;
        self.stats.persist_reads_total = self.stats.persist_reads_total.saturating_add(1);
        self.read_lock_until_ms_ro(&rtxn)
    }

    fn read_lock_until_ms_ro(&self, txn: &heed::RoTxn<'_>) -> Result<u64, DurableOpenGateError> {
        let Some(raw) = self.db.get(txn, self.cfg.record_key)? else {
            return Ok(0);
        };
        let decoded = rkyv::from_bytes::<OpenGateRecordV1, rkyv::rancor::Error>(raw)
            .map_err(|err| DurableOpenGateError::Decode(err.to_string()))?;
        Ok(decoded.lock_until_ms)
    }

    fn read_lock_until_ms_rw(&self, txn: &heed::RwTxn<'_>) -> Result<u64, DurableOpenGateError> {
        let Some(raw) = self.db.get(txn, self.cfg.record_key)? else {
            return Ok(0);
        };
        let decoded = rkyv::from_bytes::<OpenGateRecordV1, rkyv::rancor::Error>(raw)
            .map_err(|err| DurableOpenGateError::Decode(err.to_string()))?;
        Ok(decoded.lock_until_ms)
    }
}

fn duration_to_millis(value: Duration) -> u64 {
    value.as_millis().min(u128::from(u64::MAX)) as u64
}

fn millis_to_duration(value_ms: u64) -> Duration {
    Duration::from_millis(value_ms)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "shared-rate-limiter-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    #[test]
    fn allows_first_then_blocks_within_window() {
        let dir = temp_dir("open-block");
        let mut gate = DurableOpenGate::open(
            &dir,
            DurableOpenGateConfig {
                window: Duration::from_secs(1800),
                ..DurableOpenGateConfig::default()
            },
        )
        .expect("gate should open");

        let first = gate
            .try_acquire_open(Duration::from_secs(10))
            .expect("first open should be allowed");
        let second = gate
            .try_acquire_open(Duration::from_secs(20))
            .expect("second open should be blocked");

        assert!(matches!(first, OpenGateDecision::Allowed { .. }));
        assert!(matches!(second, OpenGateDecision::Blocked { .. }));
        let stats = gate.stats();
        assert_eq!(stats.allowed_total, 1);
        assert_eq!(stats.blocked_total, 1);
        assert_eq!(stats.persist_writes_total, 1);
    }

    #[test]
    fn allows_after_window_elapsed_and_persists_across_restart() {
        let dir = temp_dir("restart");
        let cfg = DurableOpenGateConfig {
            window: Duration::from_secs(30),
            ..DurableOpenGateConfig::default()
        };
        let mut gate = DurableOpenGate::open(&dir, cfg.clone()).expect("gate should open");
        let lock_until = match gate
            .try_acquire_open(Duration::from_secs(1))
            .expect("first should allow")
        {
            OpenGateDecision::Allowed { lock_until } => lock_until,
            OpenGateDecision::Blocked { .. } => panic!("first call should not be blocked"),
        };
        drop(gate);

        let mut restarted = DurableOpenGate::open(&dir, cfg).expect("gate should reopen");
        let blocked = restarted
            .try_acquire_open(Duration::from_secs(10))
            .expect("inside lock should block");
        assert!(matches!(
            blocked,
            OpenGateDecision::Blocked {
                lock_until: value
            } if value == lock_until
        ));
        let allowed = restarted
            .try_acquire_open(Duration::from_secs(31))
            .expect("after lock should allow");
        assert!(matches!(allowed, OpenGateDecision::Allowed { .. }));
    }

    #[test]
    fn spam_blocked_attempts_do_not_write_every_attempt() {
        let dir = temp_dir("spam");
        let mut gate = DurableOpenGate::open(
            &dir,
            DurableOpenGateConfig {
                window: Duration::from_secs(300),
                ..DurableOpenGateConfig::default()
            },
        )
        .expect("gate should open");

        let _ = gate
            .try_acquire_open(Duration::from_secs(0))
            .expect("seed allow should pass");
        for _ in 0..200_000 {
            let decision = gate
                .try_acquire_open(Duration::from_secs(1))
                .expect("blocked spam should be fast");
            assert!(matches!(decision, OpenGateDecision::Blocked { .. }));
        }

        let stats = gate.stats();
        assert_eq!(stats.allowed_total, 1);
        assert_eq!(stats.blocked_total, 200_000);
        assert_eq!(stats.persist_writes_total, 1);
    }

    #[test]
    fn boundary_spam_writes_once_per_window_not_per_attempt() {
        let dir = temp_dir("boundary");
        let mut gate = DurableOpenGate::open(
            &dir,
            DurableOpenGateConfig {
                window: Duration::from_secs(5),
                ..DurableOpenGateConfig::default()
            },
        )
        .expect("gate should open");

        for second in 0..20_u64 {
            let _ = gate
                .try_acquire_open(Duration::from_secs(second))
                .expect("acquire should not fail");
        }

        let stats = gate.stats();
        assert_eq!(stats.allowed_total, 4);
        assert_eq!(stats.persist_writes_total, 4);
        assert_eq!(stats.attempts_total, 20);
    }

    #[test]
    fn parallel_callers_single_grant_per_window() {
        use std::sync::{Arc, Mutex};

        let dir = temp_dir("parallel");
        let gate = Arc::new(Mutex::new(
            DurableOpenGate::open(
                &dir,
                DurableOpenGateConfig {
                    window: Duration::from_secs(60),
                    ..DurableOpenGateConfig::default()
                },
            )
            .expect("gate should open"),
        ));

        let mut joins = Vec::new();
        for _ in 0..24 {
            let cloned = gate.clone();
            joins.push(thread::spawn(move || {
                let mut guard = cloned.lock().expect("gate mutex should lock");
                guard
                    .try_acquire_open(Duration::from_secs(10))
                    .expect("acquire should work")
            }));
        }

        let mut allowed = 0_u64;
        for join in joins {
            match join.join().expect("thread should join") {
                OpenGateDecision::Allowed { .. } => allowed += 1,
                OpenGateDecision::Blocked { .. } => {}
            }
        }
        assert_eq!(allowed, 1);
    }
}
