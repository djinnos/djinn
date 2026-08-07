use djinn_provider::catalog::HealthKey;

/// Result of a `try_dispatch_to_pool` failover-chain traversal.
#[derive(Debug)]
pub(crate) enum DispatchOutcome {
    /// Successfully dispatched to a slot, on `model_id`.
    ///
    /// The accepted model is carried rather than re-derived. Callers used to
    /// recover it with `model_ids.iter().find(|m| health.is_available(..))`,
    /// which agrees with the failover chain on breaker-open skips but NOT on
    /// `PoolError::AtCapacity` or pool errors — the chain advances past those
    /// candidates and the `find` does not. On a capacity-driven failover the
    /// durable `dispatch_state.inflight_model_id` and both per-user cap
    /// counters were therefore attributed to the model that was SKIPPED: the
    /// cap under-counted the model actually running, over-counted the full one,
    /// and a restart rehydrating `inflight_model_id` resurrected the wrong
    /// model.
    Dispatched { model_id: String },
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
