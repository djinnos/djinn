/// Result of a `try_dispatch_to_pool` failover-chain traversal.
pub(crate) enum DispatchOutcome {
    /// Successfully dispatched to a slot.
    Dispatched,
    /// All candidate models are at capacity.
    AtCapacity,
    /// All failover candidates were tried but none accepted the dispatch
    /// (breaker-open or non-capacity errors); the chain is exhausted.
    Failed,
    /// The slot pool actor is dead — caller should abort.
    PoolDead,
}
