# shared-rate_limiter Architecture

## Goals

- Protocol-agnostic rate limiting core: no HTTP/WS/Discord types in the core API.
- Fast hot path with deterministic behavior.
- Two-phase permit accounting: reserve, then finalize (commit or refund).
- Support both fixed-window limits and token-bucket (credit pool + refill) limits.
- Controlled backoff from server feedback (429-style) without requiring async/threads inside the core.
- Bounded memory for key tracking and in-flight dedup helpers.
- Optional kameo integration without making kameo a mandatory dependency.

## Non-Goals

- Exactly-once delivery to external services.
- Distributed/global rate limiting across multiple processes.
- Persisted limiter state across process restarts (can be added at a higher layer later).

## Time Model

All APIs take `now: Duration` explicitly. The crate does not call the system clock on the hot path.

- The caller is responsible for providing monotonic timestamps (typically from `Instant::now()`-based deltas).
- All retry/backoff values are returned as `Duration` deltas (retry-after style).

This makes tests deterministic and avoids hidden background tasks.

## Core Types and Flow

### Config and Algorithms

The limiter is configured with `Config`:

- Fixed window: `Config::fixed_window(max_per_window, window)`
- Token bucket / credit pool: `Config::token_bucket(capacity, refill_per_sec)`

The algorithm kind is stored in `AlgorithmConfig` and is used by each tracked bucket.

### RateLimiter (Single-Owner Core)

`RateLimiter` is intentionally single-owner:

- Core methods take `&mut self` to keep the hot path simple and fast.
- Concurrency is achieved by putting the limiter behind an actor (kameo wrapper) or external sync chosen by the application.

Primary operations:

- `try_acquire(route_key, cost, now) -> Result<Permit, Deny>`
- `commit(permit, outcome, now)` consumes the permit and updates state
- `refund(permit, now)` consumes the permit and restores capacity when safe
- `apply_feedback(route_key, feedback, now)` applies 429-style feedback without needing a permit

### Permit Lifecycle (Two-Phase Accounting)

1. `try_acquire` reserves capacity and returns a `Permit` on success.
2. The caller performs the side-effect (HTTP request, WS send, etc).
3. The caller finalizes the permit using:
   - `commit(permit, Outcome::Confirmed, now)` when confirmation is observed
   - `commit(permit, Outcome::SentNoConfirm, now)` when it likely sent but no confirmation was observed
   - `refund(permit, now)` when it definitely did not send

Permits are linear: they are consumed by `commit`/`refund`, preventing double-finalization at compile time.

### Outcome (Protocol-Agnostic Classification)

`Outcome` is the protocol-agnostic classification used for accounting:

- `Confirmed`
- `NotSent`
- `SentNoConfirm`
- `RateLimitedFeedback { retry_after, scope, bucket_hint }`

Important behavior:

- `Outcome::NotSent` is treated as a refund (fixed-window and token-bucket both restore capacity).
- `Outcome::RateLimitedFeedback` can trigger a controlled backoff:
  - Global scope is handled at the `RateLimiter` level (blocks all keys).
  - Key scope is handled at the bucket level (blocks that key).

## Feedback and Backoff

### Feedback Trait

Provider adapters translate protocol-specific signals into `RateLimitFeedback`:

- HTTP: 429 + Retry-After header
- Discord: X-RateLimit-Remaining/Reset-After headers
- WS: provider-specific error frames / close codes (adapter-defined)

The core consumes these via:

- `RateLimiter::apply_feedback(route_key, &impl RateLimitFeedback, now)`

### Controlled Behavior (No Oscillation by Default)

This crate does not attempt to "auto-tune" limits (AIMD-style control loops) inside the core.
Instead:

- The limiter enforces configured throughput (fixed-window or token bucket).
- Feedback applies a bounded backoff via `blocked_until` (max of existing and new backoff).
- Repeated feedback cannot shorten an existing backoff window, preventing oscillations from alternating signals.

If you want adaptive tuning, implement it in the provider adapter layer by adjusting `Config` over time
via `RateLimiter::update_config(new_cfg, now)` (handling the `Result`) using a stable controller
(with caps, smoothing, and hysteresis).

## Slow Start

`SlowStart` optionally scales the effective capacity during the first N seconds after `started_at`.

- It is deterministic and computed from `(now - started_at)`.
- It does not require background tasks.
- It applies by scaling algorithm parameters used at decision time (effective max/window or capacity/refill).

Feedback backoff still takes precedence by denying until `blocked_until` is reached.

## Memory Bounds and Key Cardinality

The limiter can be configured with `max_keys` and an `OverflowPolicy`:

- `DenyNewKey`: refuse new keys once the cap is reached
- `EvictOldestKey`: FIFO eviction, but never evict keys with outstanding reservations

Route-to-bucket remapping (from feedback) is stored in a bounded `route_map` (best-effort bounded by `max_keys`).

## In-Flight Dedup Building Blocks (Non-kameo)

This crate provides primitives for deduplicating duplicate request ids and attaching permits through completion:

### InFlightTable

`InFlightTable<Id, Fingerprint, Waiter>` supports:

- `begin(id, fp, deadline, waiter) -> BeginResult`
  - `New`: caller performs side-effect exactly once
  - `Joined`: duplicate joined; caller must not send again
  - `PayloadMismatch`: same id, different fingerprint
  - `TableFull`: bounded max in-flight cap reached
- `complete(id) -> Vec<Waiter>`
- `expire(now) -> Vec<(Id, Vec<Waiter>)>`

Expiry uses a min-heap keyed by deadlines. The implementation includes heap compaction to avoid unbounded growth
when entries complete long before their deadlines.

### LimiterInFlight

`LimiterInFlight` is a helper that carries a `Permit` plus a deadline and provides:

- `finalize(limiter, outcome, now)` (refund on `NotSent`, commit otherwise)
- `expire(limiter, policy, now)` with an explicit policy for ambiguity

## Observability Hooks

The crate exposes `LimiterObserver` with:

- `on_deny(DenyEvent)`
- `on_feedback(FeedbackEvent)`

Applications can wire these to `tracing`, metrics, or structured logs without forcing a logging backend dependency.

## Optional kameo Integration (feature = "kameo")

The `kameo` feature provides a minimal actor wrapper: `RateLimiterActor`.

### Registration and Lookup

The actor requires a caller-provided name and registers itself during `on_start` so other components can do:

- `ActorRef::<RateLimiterActor>::lookup("the_name")`

The module also enforces a process-level singleton name (first name wins).

### Messages

The actor exposes protocol-agnostic messages:

- `AcquirePermits { key, permits, now } -> Result<AcquiredPermits, Deny>`
- `ApplyFeedback { route_key, feedback: Feedback, now } -> ()`
- `FinalizePermit { permit, outcome, now } -> ()`

This supports a clean shared-ws pattern:

1. Ask for permits before sending.
2. Tell the actor about feedback when you receive "too many requests" signals.
3. Finalize permits when the outcome is known.

## Provider Examples (Protocol-Agnostic Inputs)

This crate includes integration tests that encode real provider shapes using only numeric inputs:

- Deribit: token-bucket credit pool examples (capacity/refill/cost).
- Bybit: HTTP IP fixed-window example (600 requests / 5 seconds) and "ban" style feedback backoff.
- Discord: header-driven feedback examples (remaining=0 + reset-after).

These are examples of how an external REST/WS adapter can map provider-specific signals to:

- `Feedback` (retry-after + scope)
- `Outcome::RateLimitedFeedback` when finalizing a permit
