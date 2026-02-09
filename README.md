# shared-rate_limiter

- Protocol-agnostic, synchronous (non-async) rate limiter core.
- Deterministic time model: caller supplies `now: Duration` (no system clock on the hot path).
- Two-phase, linear permits: `try_acquire` reserves; `commit` / `refund` consumes the `Permit`.
- Fixed window limits (`Config::fixed_window`).
- Token bucket / credit pool limits (`Config::token_bucket`).
- Server feedback/backoff support (429 / Retry-After, Discord-style headers) via `apply_feedback`.
- Optional slow start ramp (`SlowStart`) and deterministic runtime config updates (`update_config`).
- Bounded memory for key tracking + overflow policies (`DenyNewKey`, `EvictOldestKey`).
- In-flight dedup building blocks: `InFlightTable`, `LimiterInFlight`.
- Observability hooks: `LimiterObserver` (deny + feedback events).
- Optional `kameo` integration behind a feature flag (`--features kameo`).

## Non-goals

- Distributed/global rate limiting across processes.
- Persisted limiter state across restarts.

## More

- Architecture notes: `docs/architecture/architecture.md`
