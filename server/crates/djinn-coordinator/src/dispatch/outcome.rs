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
    /// `exhausted_observations` preserves chain-local dispatch diagnostics only.
    /// Generic pool failures have no typed in-pod `ProviderError`, so these keys
    /// must never be applied to `HealthTracker` or used as breaker evidence.
    Failed {
        exhausted_observations: Vec<HealthKey>,
    },
    /// The slot pool actor is dead — caller should abort.
    PoolDead,
}
