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
- Durable LMDB-backed open-gate primitive (`DurableOpenGate`) for at-most-one grant per window with crash recovery.
- Observability hooks: `LimiterObserver` (deny + feedback events).

## Non-goals

- Distributed/global rate limiting across processes.
- Persisted limiter state across restarts.

## More

- Architecture notes: `docs/architecture/architecture.md`

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
