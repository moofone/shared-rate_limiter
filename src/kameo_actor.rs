//! Optional kameo actor wrapper for permit acquisition batching.
//!
//! This module is gated behind the `kameo` cargo feature to keep kameo out of the default
//! dependency tree.

use std::borrow::Cow;
use std::sync::OnceLock;
use std::time::Duration;

use kameo::actor::{Actor, ActorRef, Spawn};
use kameo::error::RegistryError;
use kameo::message::{Context, Message};

use crate::{Cost, Deny, Feedback, Key, Outcome, Permit, RateLimiter};

static SINGLETON_NAME: OnceLock<String> = OnceLock::new();

/// Request a batch of permits for a key at a given timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquirePermits {
    /// Route or bucket key (callers may pre-hash).
    pub key: Key,
    /// Number of permits to reserve (must be > 0).
    pub permits: u32,
    /// Monotonic timestamp in the same clock domain used by the limiter.
    pub now: Duration,
}

/// Successful batch acquisition.
#[derive(Debug, PartialEq, Eq)]
pub struct AcquiredPermits {
    /// A single permit whose `cost` encodes the batch size.
    pub permit: Permit,
}

/// Apply server feedback (429-style) to the limiter without requiring a permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyFeedback {
    /// Route or bucket key (callers may pre-hash).
    pub route_key: Key,
    /// Protocol-agnostic feedback payload (retry-after, scope, optional reset/remaining, remap).
    pub feedback: Feedback,
    /// Monotonic timestamp in the same clock domain used by the limiter.
    pub now: Duration,
}

/// Finalize a previously-acquired permit by reporting an outcome back to the limiter.
#[derive(Debug, PartialEq, Eq)]
pub struct FinalizePermit {
    /// The permit returned from [`AcquirePermits`].
    pub permit: Permit,
    /// Accounting classification (Confirmed/NotSent/SentNoConfirm/RateLimitedFeedback...).
    pub outcome: Outcome,
    /// Monotonic timestamp in the same clock domain used by the limiter.
    pub now: Duration,
}

/// A minimal kameo actor that owns a [`RateLimiter`].
///
/// This actor is intended as a clean migration target for global/shared rate limiting,
/// where multiple callers ask for batched permits.
pub struct RateLimiterActor {
    name: Cow<'static, str>,
    limiter: RateLimiter,
}

impl RateLimiterActor {
    /// Construct a new actor from an existing limiter.
    ///
    /// `name` is required so the actor can register itself for [`ActorRef::lookup`].
    pub fn new(name: impl Into<Cow<'static, str>>, limiter: RateLimiter) -> Self {
        Self {
            name: name.into(),
            limiter,
        }
    }

    /// Spawn the actor on the tokio runtime, returning an [`ActorRef`].
    pub fn spawn(name: impl Into<Cow<'static, str>>, limiter: RateLimiter) -> ActorRef<Self> {
        <Self as Spawn>::spawn(Self::new(name, limiter))
    }

    /// Spawn (or reuse) the process-level singleton, enforcing one actor name for the lifetime
    /// of the process.
    pub async fn spawn_singleton(
        name: impl Into<String>,
        limiter: RateLimiter,
    ) -> Result<ActorRef<Self>, SingletonError> {
        let name = name.into();

        if let Some(prev) = SINGLETON_NAME.get() {
            if prev != &name {
                return Err(SingletonError::NameMismatch {
                    expected: prev.clone(),
                    got: name,
                });
            }
        } else if SINGLETON_NAME.set(name.clone()).is_err() {
            let prev = SINGLETON_NAME.get().expect("just set by another thread");
            if prev != &name {
                return Err(SingletonError::NameMismatch {
                    expected: prev.clone(),
                    got: name,
                });
            }
        }

        if let Some(existing) = ActorRef::<Self>::lookup(name.as_str())? {
            return Ok(existing);
        }

        let actor_ref = Self::spawn(Cow::Owned(name.clone()), limiter);
        #[derive(Debug)]
        enum StartupStatus {
            Ok,
            NameAlreadyRegistered,
            Other(String),
        }

        let startup = actor_ref
            .wait_for_startup_with_result(|res| match res {
                Ok(()) => StartupStatus::Ok,
                Err(kameo::error::HookError::Error(&RateLimiterActorError::Registry(
                    RegistryError::NameAlreadyRegistered,
                ))) => StartupStatus::NameAlreadyRegistered,
                Err(err) => StartupStatus::Other(format!("{err:?}")),
            })
            .await;

        match startup {
            StartupStatus::Ok => {}
            StartupStatus::NameAlreadyRegistered => {
                if let Some(existing) = ActorRef::<Self>::lookup(name.as_str())? {
                    return Ok(existing);
                }
                return Err(SingletonError::StartupFailed(
                    "NameAlreadyRegistered but lookup returned None".to_string(),
                ));
            }
            StartupStatus::Other(err) => return Err(SingletonError::StartupFailed(err)),
        }

        // If startup failed because of a concurrent initializer, prefer the registered actor.
        if let Some(existing) = ActorRef::<Self>::lookup(name.as_str())? {
            return Ok(existing);
        }

        Ok(actor_ref)
    }
}

/// Startup error for [`RateLimiterActor`].
#[derive(Debug)]
pub enum RateLimiterActorError {
    /// A singleton name has already been established for this process and does not match.
    SingletonNameMismatch {
        /// Name already chosen for the singleton.
        expected: String,
        /// Name requested by the new actor.
        got: String,
    },
    /// Actor registry error.
    Registry(RegistryError),
}

impl From<RegistryError> for RateLimiterActorError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}

impl Actor for RateLimiterActor {
    type Args = Self;
    type Error = RateLimiterActorError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // Enforce process-level singleton: once a name has been chosen, no other RateLimiterActor
        // can start under a different name.
        let got = args.name.to_string();
        if let Some(prev) = SINGLETON_NAME.get() {
            if prev != &got {
                return Err(RateLimiterActorError::SingletonNameMismatch {
                    expected: prev.clone(),
                    got,
                });
            }
        } else if SINGLETON_NAME.set(got.clone()).is_err() {
            let prev = SINGLETON_NAME.get().expect("just set by another thread");
            if prev != &got {
                return Err(RateLimiterActorError::SingletonNameMismatch {
                    expected: prev.clone(),
                    got,
                });
            }
        }

        // Register on startup to support ActorRef::lookup(name).
        actor_ref.register(args.name.clone())?;
        Ok(args)
    }
}

impl Message<AcquirePermits> for RateLimiterActor {
    type Reply = Result<AcquiredPermits, Deny>;

    async fn handle(
        &mut self,
        msg: AcquirePermits,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let Some(cost) = Cost::new(msg.permits) else {
            return Err(Deny {
                retry_after: Duration::ZERO,
            });
        };
        let permit = self.limiter.try_acquire(msg.key, cost, msg.now)?;
        Ok(AcquiredPermits { permit })
    }
}

impl Message<ApplyFeedback> for RateLimiterActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: ApplyFeedback,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.limiter
            .apply_feedback(msg.route_key, &msg.feedback, msg.now);
    }
}

impl Message<FinalizePermit> for RateLimiterActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: FinalizePermit,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // `commit` treats Outcome::NotSent as a defensive refund for fixed-window and token bucket.
        self.limiter.commit(msg.permit, msg.outcome, msg.now);
    }
}

/// Error returned by [`RateLimiterActor::spawn_singleton`].
#[derive(Debug)]
pub enum SingletonError {
    /// The singleton was already initialized with a different name.
    NameMismatch {
        /// Name used by the already-initialized singleton.
        expected: String,
        /// Name requested by the current initializer.
        got: String,
    },
    /// Startup failed (includes registry failures).
    StartupFailed(String),
    /// Registry access failed.
    Registry(RegistryError),
}

impl From<RegistryError> for SingletonError {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Outcome};

    const NAME: &str = "shared_rate_limiter_kameo_test_singleton";

    #[tokio::test]
    async fn kameo_actor_smoke() {
        // Each `#[tokio::test]` uses its own runtime. Since this module enforces a process-level
        // singleton, all kameo actor checks must run within a single test/runtime.
        let cfg = Config::fixed_window(10, Duration::from_secs(10)).unwrap();
        let actor = RateLimiterActor::spawn_singleton(NAME.to_string(), RateLimiter::new(cfg))
            .await
            .unwrap();
        actor
            .wait_for_startup_with_result(|res| assert!(res.is_ok()))
            .await;

        // Batch acquisition + deny path.
        let ok = actor
            .ask(AcquirePermits {
                key: 1001,
                permits: 2,
                now: Duration::from_secs(0),
            })
            .await
            .unwrap();
        assert_eq!(ok.permit.key(), 1001);
        assert_eq!(ok.permit.cost().get(), 2);

        // Refund the first batch to prove finalization works.
        actor
            .ask(FinalizePermit {
                permit: ok.permit,
                outcome: Outcome::NotSent,
                now: Duration::from_secs(0),
            })
            .await
            .unwrap();

        // Should allow a full batch again after refund.
        let ok2 = actor
            .ask(AcquirePermits {
                key: 1001,
                permits: 10,
                now: Duration::from_secs(0),
            })
            .await
            .unwrap();

        // The actor currently reserved 10 permits in total for key=1001; next should deny.
        let err = actor
            .ask(AcquirePermits {
                key: 1001,
                permits: 1,
                now: Duration::from_secs(0),
            })
            .await
            .unwrap_err();
        match err {
            kameo::error::SendError::HandlerError(d) => {
                assert_eq!(d.retry_after, Duration::from_secs(10));
            }
            other => panic!("unexpected send error: {other:?}"),
        }

        let _ = ok2;

        // Feedback blocks key.
        actor
            .ask(ApplyFeedback {
                route_key: 2002,
                feedback: Feedback::new(Duration::from_secs(3), crate::FeedbackScope::Key),
                now: Duration::from_secs(0),
            })
            .await
            .unwrap();

        let err = actor
            .ask(AcquirePermits {
                key: 2002,
                permits: 1,
                now: Duration::from_secs(1),
            })
            .await
            .unwrap_err();
        match err {
            kameo::error::SendError::HandlerError(d) => {
                assert_eq!(d.retry_after, Duration::from_secs(2));
            }
            other => panic!("unexpected send error: {other:?}"),
        }

        // Lookup finds the singleton.
        let found = ActorRef::<RateLimiterActor>::lookup(NAME)
            .unwrap()
            .expect("registered actor should be discoverable");
        assert_eq!(found.id(), actor.id());
    }
}
