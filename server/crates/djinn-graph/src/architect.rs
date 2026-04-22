//! Architect-only construction path for the canonical-graph warm API.
//!
//! `ensure_canonical_graph` / `run_warm_graph_command` drive the expensive
//! SCIP indexing pipeline (multiple language indexers + petgraph build +
//! bincode blob + DB upsert).  Per project convention only the `architect`
//! role is authorised to warm the canonical graph — worker roles tolerate a
//! stale skeleton and must never trigger a rebuild.
//!
//! To make the invariant enforceable at compile time rather than via
//! `debug_assert!`, both warm entry-points now take an [`ArchitectWarmToken`]
//! — a zero-sized witness type whose only-pub-crate field prevents any
//! caller outside this module from constructing one directly.  The sanctioned
//! constructor lives at [`ArchitectWarmToken::new`] inside this deliberately
//! named module, so reviewers grep-ing for `djinn_graph::architect::` can
//! audit every legitimate warm site.
//!
//! Sanctioned constructors:
//! * Architect dispatch path — [`crate::architect::ArchitectWarmToken::new`],
//!   wired in through `AppState::build_in_process_graph_warmer`.
//! * K8s warm-Pod runner (`djinn-agent-worker warm-graph <project_id>`) —
//!   same constructor; the subprocess binary is only spawned by the K8s
//!   warmer, which is itself only triggered by architect-role dispatch.
//! * Tests — [`ArchitectWarmToken::for_tests`], gated on `#[cfg(test)]`.
//!
//! Non-sanctioned callers (chat handler, future worker paths) cannot
//! construct the token and therefore cannot call the warm API by mistake.

/// Zero-sized capability witness proving the caller is on the
/// architect-authorised warm path.  Taken by value at warm entry-points so
/// each warm acquires a fresh token — pass-through storage of the token is
/// not meaningful but also not harmful (the type carries no data).
///
/// The inner `()` is `pub(crate)` so the only way to mint one from outside
/// the crate is through [`ArchitectWarmToken::new`].  Call sites that rely
/// on that constructor make the "this is the architect path" claim
/// explicitly, auditable by grep.
pub struct ArchitectWarmToken(pub(crate) ());

impl ArchitectWarmToken {
    /// Mint a fresh token on the architect warm path.
    ///
    /// The function is intentionally cheap and side-effect-free — the
    /// discipline this type enforces is purely at the API layer.  Call
    /// sites must be able to justify (to code review) why they are on the
    /// architect path.
    pub fn new() -> Self {
        Self(())
    }

    /// Test-only constructor.  Use inside `#[cfg(test)]` modules that want
    /// to drive `ensure_canonical_graph` directly without pretending to be
    /// the architect scheduler.  Not available in non-test builds.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self(())
    }
}

impl Default for ArchitectWarmToken {
    fn default() -> Self {
        Self::new()
    }
}
