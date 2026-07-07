use djinn_provider::catalog::HealthKey;

/// Result of a `try_dispatch_to_pool` failover-chain traversal.
#[derive(Debug)]
pub(crate) enum DispatchOutcome {
    /// Successfully dispatched to a slot.
    Dispatched,
    /// All candidate models are at capacity.
    AtCapacity,
    /// All failover candidates were tried but none accepted the dispatch
    /// (breaker-open or non-capacity errors); the chain is exhausted.
    ///
    /// `exhausted_observations` is the chain-scoped list of `(scope, model)`
    /// keys for candidates that this dispatch attempt observed to fail
    /// (recorded via [`HealthTracker::record_failure_observation`]). It is
    /// returned here so the *current* chain's exhaustion is the only chain
    /// whose deferred breaker checks get applied — see
    /// [`CoordinatorActor::apply_chain_exhaustion_side_effects`]. Successful
    /// fallbacks discard this list and never trigger breaker side effects
    /// for an earlier candidate.
    Failed {
        exhausted_observations: Vec<HealthKey>,
    },
    /// The slot pool actor is dead — caller should abort.
    PoolDead,
}
