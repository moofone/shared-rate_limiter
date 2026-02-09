use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::hash::Hash;
use std::time::Duration;

use crate::hash::HashMap;
use crate::types::{Outcome, Permit};

/// Result of [`InFlightTable::begin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginResult {
    /// A new entry was created; the caller should perform the side-effect exactly once.
    New,
    /// An existing entry was found and the waiter joined; the caller must not perform the side-effect again.
    Joined,
    /// The entry exists but the fingerprint does not match.
    PayloadMismatch,
    /// The table is at capacity and a new entry could not be created.
    TableFull,
}

/// A typed payload mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadMismatch;

#[derive(Debug)]
enum Waiters<W> {
    One(W),
    Many(Vec<W>),
}

impl<W> Waiters<W> {
    fn push(&mut self, w: W) {
        match self {
            Waiters::One(_) => {
                let old = match std::mem::replace(self, Waiters::Many(Vec::new())) {
                    Waiters::One(w0) => w0,
                    Waiters::Many(_) => unreachable!("just replaced with Many"),
                };
                if let Waiters::Many(v) = self {
                    v.push(old);
                    v.push(w);
                }
            }
            Waiters::Many(v) => v.push(w),
        }
    }

    fn into_vec(self) -> Vec<W> {
        match self {
            Waiters::One(w) => vec![w],
            Waiters::Many(v) => v,
        }
    }
}

#[derive(Debug)]
struct EntryState<F, W> {
    fp: F,
    deadline: Duration,
    waiters: Waiters<W>,
    seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeapItem<Id> {
    deadline: Duration,
    seq: u64,
    id: Id,
}

impl<Id: Ord> Ord for HeapItem<Id> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse (min-heap) via wrapping in `Reverse` at the heap site.
        (self.deadline, self.seq, &self.id).cmp(&(other.deadline, other.seq, &other.id))
    }
}

impl<Id: Ord> PartialOrd for HeapItem<Id> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// In-flight dedup table keyed by `Id`, with `Fingerprint` equality used to reject payload mismatches.
///
/// Common-case waiter storage is allocation-free: the first waiter is stored inline, and only on
/// the second join do we allocate a `Vec`.
#[derive(Debug)]
pub struct InFlightTable<Id, Fingerprint, Waiter> {
    max_in_flight: usize,
    seq: u64,
    map: HashMap<Id, EntryState<Fingerprint, Waiter>>,
    heap: BinaryHeap<Reverse<HeapItem<Id>>>,
}

impl<Id, Fingerprint, Waiter> InFlightTable<Id, Fingerprint, Waiter>
where
    Id: Copy + Eq + Hash + Ord,
    Fingerprint: Copy + Eq,
{
    /// Create a new table.
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            max_in_flight,
            seq: 0,
            map: HashMap::default(),
            heap: BinaryHeap::new(),
        }
    }

    /// Current number of entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns `true` if the table has no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Begin (or join) an in-flight entry.
    pub fn begin(
        &mut self,
        id: Id,
        fp: Fingerprint,
        deadline: Duration,
        waiter: Waiter,
    ) -> BeginResult {
        if let Some(st) = self.map.get_mut(&id) {
            if st.fp != fp {
                return BeginResult::PayloadMismatch;
            }
            st.waiters.push(waiter);
            return BeginResult::Joined;
        }

        if self.map.len() >= self.max_in_flight {
            return BeginResult::TableFull;
        }

        let seq = self.next_seq();
        self.heap.push(Reverse(HeapItem { deadline, seq, id }));
        self.map.insert(
            id,
            EntryState {
                fp,
                deadline,
                waiters: Waiters::One(waiter),
                seq,
            },
        );
        self.maybe_compact_heap();
        BeginResult::New
    }

    /// Complete and drain waiters for an entry.
    pub fn complete(&mut self, id: Id) -> Vec<Waiter> {
        let out = self
            .map
            .remove(&id)
            .map(|e| e.waiters.into_vec())
            .unwrap_or_default();
        // `BinaryHeap` doesn't support removing arbitrary items. We use a bounded lazy deletion
        // strategy and compact when the heap grows too large relative to the live map.
        self.maybe_compact_heap();
        out
    }

    /// Expire entries with `deadline <= now`, returning drained waiters for each expired id.
    ///
    /// This is heap-based; it will pop as many expired entries as are currently due.
    pub fn expire(&mut self, now: Duration) -> Vec<(Id, Vec<Waiter>)> {
        let mut out = Vec::new();
        while let Some(Reverse(top)) = self.heap.peek().copied() {
            if top.deadline > now {
                break;
            }
            self.heap.pop();
            let Some(cur) = self.map.get(&top.id) else {
                continue;
            };
            if cur.seq != top.seq {
                continue;
            }
            if let Some(e) = self.map.remove(&top.id) {
                out.push((top.id, e.waiters.into_vec()));
            }
        }
        self.maybe_compact_heap();
        out
    }

    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn maybe_compact_heap(&mut self) {
        // Without compaction, the heap can accumulate stale entries when `complete()` is called
        // before deadlines are reached. Keep memory bounded by periodically rebuilding the heap
        // from the live map when it grows too large.
        let live = self.map.len();
        if live == 0 {
            // Common in steady-state: lots of short-lived in-flight ids that complete long before
            // their deadlines. Clearing is safe because there are no live entries to expire.
            self.heap.clear();
            return;
        }
        let threshold = live.saturating_mul(4) + 64;
        if self.heap.len() > threshold {
            self.compact_heap();
        }
    }

    fn compact_heap(&mut self) {
        let mut heap = BinaryHeap::with_capacity(self.map.len());
        for (id, st) in &self.map {
            heap.push(Reverse(HeapItem {
                deadline: st.deadline,
                seq: st.seq,
                id: *id,
            }));
        }
        self.heap = heap;
    }
}

/// Policy to apply on deadline expiry for permit-attached entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimiterInFlightExpiryPolicy {
    /// Default: treat expiry as ambiguous delivery and commit `SentNoConfirm` (no refund).
    CommitSentNoConfirm,
    /// Refund on expiry (only safe when the application can prove the operation did not send).
    Refund,
}

/// In-flight entry helper that carries a limiter `Permit` and an expiry policy.
#[derive(Debug)]
pub struct LimiterInFlight<W> {
    /// Reserved capacity.
    pub permit: Permit,
    /// Deadline for this in-flight operation.
    pub deadline: Duration,
    waiters: Waiters<W>,
}

impl<W> LimiterInFlight<W> {
    /// Create a new permit-attached in-flight entry with a single waiter (inline, allocation-free).
    pub fn new(permit: Permit, deadline: Duration, waiter: W) -> Self {
        Self {
            permit,
            deadline,
            waiters: Waiters::One(waiter),
        }
    }

    /// Join an additional waiter (allocates only on the second waiter).
    pub fn join(&mut self, waiter: W) {
        self.waiters.push(waiter);
    }

    /// Complete and drain waiters.
    pub fn complete(self) -> Vec<W> {
        self.waiters.into_vec()
    }

    /// Finalize the permit for a known outcome and drain waiters.
    pub fn finalize(
        self,
        limiter: &mut crate::RateLimiter,
        outcome: Outcome,
        now: Duration,
    ) -> Vec<W> {
        match outcome {
            Outcome::NotSent => limiter.refund(self.permit, now),
            _ => limiter.commit(self.permit, outcome, now),
        }
        self.waiters.into_vec()
    }

    /// Expire this in-flight entry, finalizing the permit according to `policy`, and drain waiters.
    pub fn expire(
        self,
        limiter: &mut crate::RateLimiter,
        policy: LimiterInFlightExpiryPolicy,
        now: Duration,
    ) -> Vec<W> {
        match policy {
            LimiterInFlightExpiryPolicy::CommitSentNoConfirm => {
                limiter.commit(self.permit, Outcome::SentNoConfirm, now);
            }
            LimiterInFlightExpiryPolicy::Refund => {
                limiter.refund(self.permit, now);
            }
        }
        self.waiters.into_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Cost, Outcome, RateLimiter};

    #[test]
    fn dedup_new_then_join_same_fp() {
        let mut t: InFlightTable<u64, u64, u64> = InFlightTable::new(10);
        assert_eq!(t.begin(1, 7, Duration::from_secs(5), 10), BeginResult::New);
        assert_eq!(
            t.begin(1, 7, Duration::from_secs(5), 11),
            BeginResult::Joined
        );
        let ws = t.complete(1);
        assert_eq!(ws, vec![10, 11]);
    }

    #[test]
    fn dedup_payload_mismatch() {
        let mut t: InFlightTable<u64, u64, u64> = InFlightTable::new(10);
        assert_eq!(t.begin(1, 7, Duration::from_secs(5), 10), BeginResult::New);
        assert_eq!(
            t.begin(1, 8, Duration::from_secs(5), 11),
            BeginResult::PayloadMismatch
        );
        let ws = t.complete(1);
        assert_eq!(ws, vec![10]);
    }

    #[test]
    fn table_full_enforced() {
        let mut t: InFlightTable<u64, u64, u64> = InFlightTable::new(1);
        assert_eq!(t.begin(1, 7, Duration::from_secs(5), 10), BeginResult::New);
        assert_eq!(
            t.begin(2, 7, Duration::from_secs(5), 20),
            BeginResult::TableFull
        );
    }

    #[test]
    fn expire_removes_and_returns_waiters() {
        let mut t: InFlightTable<u64, u64, u64> = InFlightTable::new(10);
        t.begin(1, 7, Duration::from_secs(5), 10);
        t.begin(2, 7, Duration::from_secs(2), 20);
        let expired = t.expire(Duration::from_secs(3));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, 2);
        assert_eq!(expired[0].1, vec![20]);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn heap_does_not_grow_unbounded_when_entries_complete_before_deadlines() {
        let mut t: InFlightTable<u64, u64, ()> = InFlightTable::new(10_000);
        let deadline = Duration::from_secs(60);

        // If we push a heap item per begin and always complete before expiry, stale heap items
        // would accumulate without periodic compaction.
        for i in 0..130 {
            assert_eq!(t.begin(i, 1, deadline, ()), BeginResult::New);
            t.complete(i);
        }

        assert!(t.is_empty());
        assert!(
            t.heap.is_empty(),
            "heap should be compacted back to empty when no live entries remain"
        );
    }

    #[test]
    fn permit_attachment_refund_on_not_sent() {
        let cfg = Config::fixed_window(1, Duration::from_secs(10)).unwrap();
        let mut rl = RateLimiter::new(cfg);
        let now = Duration::from_secs(0);
        let p = rl.try_acquire(1, Cost::ONE, now).unwrap();

        let entry = LimiterInFlight::new(p, Duration::from_secs(1), ());
        entry.finalize(&mut rl, Outcome::NotSent, now);

        // Refunded; should allow another acquire in the same window.
        let _p2 = rl.try_acquire(1, Cost::ONE, now).unwrap();
    }

    #[test]
    fn permit_attachment_no_refund_on_sent_no_confirm_or_expiry_by_default() {
        let cfg = Config::fixed_window(1, Duration::from_secs(10)).unwrap();
        let mut rl = RateLimiter::new(cfg);
        let now = Duration::from_secs(0);

        let p = rl.try_acquire(1, Cost::ONE, now).unwrap();
        let entry = LimiterInFlight::new(p, Duration::from_secs(1), ());
        entry.finalize(&mut rl, Outcome::SentNoConfirm, now);
        assert!(rl.try_acquire(1, Cost::ONE, now).is_err());

        // New window for expiry check.
        let mut rl = RateLimiter::new(Config::fixed_window(1, Duration::from_secs(10)).unwrap());
        let p = rl.try_acquire(1, Cost::ONE, now).unwrap();
        let entry = LimiterInFlight::new(p, Duration::from_secs(1), ());
        entry.expire(
            &mut rl,
            LimiterInFlightExpiryPolicy::CommitSentNoConfirm,
            now,
        );
        assert!(rl.try_acquire(1, Cost::ONE, now).is_err());
    }
}
