//! Deterministic "client limiter vs server-side model" tests.
//!
//! The goal is to prove correctness without self-validation: the "server" algorithms below are
//! independent implementations used to cross-check the crate's behavior for the same inputs.

use std::collections::HashMap;
use std::time::Duration;

use shared_rate_limiter::{Config, Cost, Outcome, RateLimiter, Scope, SlowStart};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerDecision {
    Ok,
    RateLimited { retry_after: Duration, scope: Scope },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerAlgo {
    FixedWindow {
        max_per_window: u32,
        window: Duration,
    },
    TokenBucket {
        capacity: u64,
        refill_per_sec: u64,
    },
}

#[derive(Debug, Clone)]
struct ServerKeyState {
    // Fixed window
    window_start: Duration,
    spent: u32,

    // Token bucket
    last_update: Duration,
    credits: u64,

    blocked_until: Option<Duration>,
}

impl ServerKeyState {
    fn new(now: Duration, algo: ServerAlgo) -> Self {
        match algo {
            ServerAlgo::FixedWindow { window, .. } => Self {
                window_start: window_start_for(now, window),
                spent: 0,
                last_update: Duration::ZERO,
                credits: 0,
                blocked_until: None,
            },
            ServerAlgo::TokenBucket { capacity, .. } => Self {
                window_start: Duration::ZERO,
                spent: 0,
                last_update: now,
                credits: capacity,
                blocked_until: None,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct DeterministicServer {
    algo: ServerAlgo,
    keys: HashMap<u64, ServerKeyState>,
    global_blocked_until: Option<Duration>,
}

impl DeterministicServer {
    fn new(algo: ServerAlgo) -> Self {
        Self {
            algo,
            keys: HashMap::new(),
            global_blocked_until: None,
        }
    }

    fn key_mut(&mut self, key: u64, now: Duration) -> &mut ServerKeyState {
        self.keys
            .entry(key)
            .or_insert_with(|| ServerKeyState::new(now, self.algo))
    }

    fn decide(&mut self, key: u64, cost: Cost, now: Duration) -> ServerDecision {
        let algo = self.algo;
        if let Some(until) = self.global_blocked_until
            && now < until
        {
            return ServerDecision::RateLimited {
                retry_after: until - now,
                scope: Scope::Global,
            };
        }

        let st = self.key_mut(key, now);
        if let Some(until) = st.blocked_until
            && now < until
        {
            return ServerDecision::RateLimited {
                retry_after: until - now,
                scope: Scope::Key,
            };
        }

        match algo {
            ServerAlgo::FixedWindow {
                max_per_window,
                window,
            } => {
                // Advance window.
                let end = st.window_start.saturating_add(window);
                if now >= end {
                    st.window_start = window_start_for(now, window);
                    st.spent = 0;
                }

                let want = cost.get();
                if want > max_per_window {
                    return ServerDecision::RateLimited {
                        retry_after: Duration::MAX,
                        scope: Scope::Key,
                    };
                }

                if st.spent.saturating_add(want) > max_per_window {
                    let end = st.window_start.saturating_add(window);
                    return ServerDecision::RateLimited {
                        retry_after: end.saturating_sub(now),
                        scope: Scope::Key,
                    };
                }

                ServerDecision::Ok
            }
            ServerAlgo::TokenBucket {
                capacity,
                refill_per_sec,
            } => {
                let want = cost.get() as u64;
                if want > capacity {
                    return ServerDecision::RateLimited {
                        retry_after: Duration::MAX,
                        scope: Scope::Key,
                    };
                }
                // Refill.
                if now > st.last_update {
                    let delta = now - st.last_update;
                    st.credits = (st
                        .credits
                        .saturating_add(credits_added(delta, refill_per_sec)))
                    .min(capacity);
                    st.last_update = now;
                }

                if st.credits >= want {
                    ServerDecision::Ok
                } else if refill_per_sec == 0 {
                    ServerDecision::RateLimited {
                        retry_after: Duration::MAX,
                        scope: Scope::Key,
                    }
                } else {
                    let deficit = want - st.credits;
                    ServerDecision::RateLimited {
                        retry_after: retry_after_for_deficit(deficit, refill_per_sec),
                        scope: Scope::Key,
                    }
                }
            }
        }
    }

    fn apply_outcome(&mut self, key: u64, cost: Cost, outcome: Outcome, now: Duration) {
        let algo = self.algo;
        let st = self.key_mut(key, now);
        match algo {
            ServerAlgo::FixedWindow { window, .. } => {
                // Advance window for accounting.
                let end = st.window_start.saturating_add(window);
                if now >= end {
                    st.window_start = window_start_for(now, window);
                    st.spent = 0;
                }
            }
            ServerAlgo::TokenBucket {
                capacity,
                refill_per_sec,
            } => {
                // Refill before applying token changes.
                if now > st.last_update {
                    let delta = now - st.last_update;
                    st.credits = (st
                        .credits
                        .saturating_add(credits_added(delta, refill_per_sec)))
                    .min(capacity);
                    st.last_update = now;
                }
            }
        }

        match outcome {
            Outcome::NotSent => {
                // Server didn't see it; no accounting changes.
            }
            Outcome::Confirmed | Outcome::SentNoConfirm => match algo {
                ServerAlgo::FixedWindow { .. } => {
                    st.spent = st.spent.saturating_add(cost.get());
                }
                ServerAlgo::TokenBucket { .. } => {
                    st.credits = st.credits.saturating_sub(cost.get() as u64);
                }
            },
            Outcome::RateLimitedFeedback {
                retry_after, scope, ..
            } => {
                // Many providers still count 429-like events; model it as "spent" plus an enforced backoff.
                match algo {
                    ServerAlgo::FixedWindow { .. } => {
                        st.spent = st.spent.saturating_add(cost.get());
                    }
                    ServerAlgo::TokenBucket { .. } => {
                        st.credits = st.credits.saturating_sub(cost.get() as u64);
                    }
                }

                let until = now.saturating_add(retry_after);
                match scope {
                    Scope::Global => {
                        self.global_blocked_until = Some(
                            self.global_blocked_until
                                .map_or(until, |cur| cur.max(until)),
                        );
                    }
                    Scope::Key => {
                        st.blocked_until =
                            Some(st.blocked_until.map_or(until, |cur| cur.max(until)));
                    }
                }
            }
        }
    }
}

fn window_start_for(now: Duration, window: Duration) -> Duration {
    debug_assert!(!window.is_zero());
    let w = window.as_nanos();
    let n = now.as_nanos();
    let start = (n / w) * w;
    let secs = (start / 1_000_000_000) as u64;
    let nanos = (start % 1_000_000_000) as u32;
    Duration::new(secs, nanos)
}

fn credits_added(delta: Duration, refill_per_sec: u64) -> u64 {
    if refill_per_sec == 0 {
        return 0;
    }
    let nanos = delta.as_nanos();
    let add = (nanos.saturating_mul(refill_per_sec as u128)) / 1_000_000_000u128;
    add.min(u64::MAX as u128) as u64
}

fn retry_after_for_deficit(deficit: u64, refill_per_sec: u64) -> Duration {
    let num = (deficit as u128).saturating_mul(1_000_000_000u128);
    let denom = refill_per_sec as u128;
    let nanos = num.div_ceil(denom);
    let secs = (nanos / 1_000_000_000) as u64;
    let sub = (nanos % 1_000_000_000) as u32;
    Duration::new(secs, sub)
}

fn drive_trace(
    mut client: RateLimiter,
    mut server: DeterministicServer,
    key: u64,
    attempts: &[(Duration, Cost)],
) {
    for (now, cost) in attempts.iter().copied() {
        let server_decision = server.decide(key, cost, now);
        let client_res = client.try_acquire(key, cost, now);

        match (client_res, server_decision) {
            (Ok(p), ServerDecision::Ok) => {
                // Send succeeded.
                server.apply_outcome(key, cost, Outcome::Confirmed, now);
                client.commit(p, Outcome::Confirmed, now);
            }
            (Ok(p), ServerDecision::RateLimited { retry_after, scope }) => {
                // Client thought it was ok, but server rate-limited. This should only occur in
                // tests designed to exercise feedback/backoff convergence.
                let outcome = Outcome::RateLimitedFeedback {
                    retry_after,
                    scope,
                    bucket_hint: None,
                };
                server.apply_outcome(key, cost, outcome.clone(), now);
                client.commit(p, outcome, now);
            }
            (Err(deny), ServerDecision::RateLimited { retry_after, .. }) => {
                // Client did not send. The recommended retry-after should match the server's model.
                assert_eq!(
                    deny.retry_after, retry_after,
                    "retry_after mismatch at now={now:?}"
                );
            }
            (Err(_deny), ServerDecision::Ok) => {
                // Client is allowed to be conservative, but if we're using identical algorithms/config,
                // this is a bug in the client's deny logic for this trace.
                panic!("client denied but server would accept at now={now:?}");
            }
        }
    }
}

#[test]
fn fixed_window_deterministic_just_under_just_over_boundary() {
    let cfg = Config::fixed_window(3, Duration::from_secs(10)).unwrap();
    let client = RateLimiter::new(cfg);
    let server = DeterministicServer::new(ServerAlgo::FixedWindow {
        max_per_window: 3,
        window: Duration::from_secs(10),
    });

    // 3 in the window ok, 4th denied until the boundary; verify near-edge behavior.
    let attempts = [
        (Duration::from_secs(0), Cost::ONE),
        (Duration::from_secs(0), Cost::ONE),
        (Duration::from_secs(0), Cost::ONE),
        (Duration::from_secs(0), Cost::ONE),
        (Duration::from_secs(9), Cost::ONE),
        (Duration::from_nanos(9_999_999_999), Cost::ONE),
        (Duration::from_secs(10), Cost::ONE),
    ];

    drive_trace(client, server, 42, &attempts);
}

#[test]
fn fixed_window_unachievable_cost_is_permanent_deny() {
    let cfg = Config::fixed_window(5, Duration::from_secs(10)).unwrap();
    let mut client = RateLimiter::new(cfg);
    let mut server = DeterministicServer::new(ServerAlgo::FixedWindow {
        max_per_window: 5,
        window: Duration::from_secs(10),
    });
    let now = Duration::from_secs(0);
    let cost = Cost::new(6).unwrap();

    let server_decision = server.decide(1, cost, now);
    let client_res = client.try_acquire(1, cost, now);

    match (client_res, server_decision) {
        (Err(d), ServerDecision::RateLimited { retry_after, .. }) => {
            assert_eq!(d.retry_after, retry_after)
        }
        other => panic!("unexpected decision pair: {other:?}"),
    }
}

#[test]
fn token_bucket_deterministic_burst_then_minimal_backoff() {
    let client = RateLimiter::new(Config::token_bucket(10, 10));
    let server = DeterministicServer::new(ServerAlgo::TokenBucket {
        capacity: 10,
        refill_per_sec: 10,
    });

    // Burst 10 at t=0, then 1 more should require 100ms (10 credits/sec, deficit=1 credit).
    let mut attempts = Vec::new();
    for _ in 0..10 {
        attempts.push((Duration::from_secs(0), Cost::ONE));
    }
    attempts.push((Duration::from_secs(0), Cost::ONE));
    attempts.push((Duration::from_millis(99), Cost::ONE));
    attempts.push((Duration::from_millis(100), Cost::ONE));

    drive_trace(client, server, 7, &attempts);
}

#[test]
fn slow_start_is_conservative_and_never_violates_server_fixed_window() {
    let cfg = Config::fixed_window(10, Duration::from_secs(10))
        .unwrap()
        .with_slow_start(SlowStart::new(Duration::from_secs(10)));
    let mut client = RateLimiter::new_at(cfg, Duration::from_secs(0));

    let mut server = DeterministicServer::new(ServerAlgo::FixedWindow {
        max_per_window: 10,
        window: Duration::from_secs(10),
    });

    // Try to send 1 request per second for 20 seconds. Client may deny early due to slow-start,
    // but any allowed send must be accepted by the server.
    for t in 0..=20u64 {
        let now = Duration::from_secs(t);
        match client.try_acquire(1, Cost::ONE, now) {
            Ok(p) => {
                let sd = server.decide(1, Cost::ONE, now);
                assert_eq!(sd, ServerDecision::Ok, "server rejected at t={t}");
                server.apply_outcome(1, Cost::ONE, Outcome::Confirmed, now);
                client.commit(p, Outcome::Confirmed, now);
            }
            Err(_d) => {
                // Denied by client; no send.
            }
        }
    }
}

#[test]
fn feedback_backoff_converges_when_client_is_too_optimistic() {
    // Client thinks it can do 4/10s; server only allows 3/10s. The 4th attempt should be 429'ed
    // by the server with retry_after until the window boundary, and the client should back off
    // deterministically after committing the feedback outcome.
    let client = RateLimiter::new(Config::fixed_window(4, Duration::from_secs(10)).unwrap());
    let server = DeterministicServer::new(ServerAlgo::FixedWindow {
        max_per_window: 3,
        window: Duration::from_secs(10),
    });

    let attempts = [
        (Duration::from_secs(0), Cost::ONE),
        (Duration::from_secs(0), Cost::ONE),
        (Duration::from_secs(0), Cost::ONE),
        (Duration::from_secs(0), Cost::ONE), // server rate-limits here
        (Duration::from_secs(1), Cost::ONE), // client must now be blocked due to feedback
        (Duration::from_secs(10), Cost::ONE), // new window, should recover
    ];

    drive_trace(client, server, 99, &attempts);
}

#[test]
fn refund_not_sent_keeps_client_and_server_in_sync() {
    let cfg = Config::fixed_window(1, Duration::from_secs(10)).unwrap();
    let mut client = RateLimiter::new(cfg);
    let mut server = DeterministicServer::new(ServerAlgo::FixedWindow {
        max_per_window: 1,
        window: Duration::from_secs(10),
    });
    let now = Duration::from_secs(0);

    let p = client.try_acquire(1, Cost::ONE, now).unwrap();

    // Simulate a local write failure: not sent, so both sides should keep capacity.
    client.refund(p, now);
    // No server outcome applied.

    // Should still allow within the same window.
    let p2 = client.try_acquire(1, Cost::ONE, now).unwrap();
    assert_eq!(server.decide(1, Cost::ONE, now), ServerDecision::Ok);
    server.apply_outcome(1, Cost::ONE, Outcome::Confirmed, now);
    client.commit(p2, Outcome::Confirmed, now);
}
