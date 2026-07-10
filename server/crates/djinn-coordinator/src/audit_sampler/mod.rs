//! # Audit sampler — immutable sample frames and replayable draws
//!
//! This module builds sealed sample-frame revisions from persisted
//! merged-change facts (produced by the merged-change ledger projection)
//! and implements deterministic replayable random draws using a documented
//! algorithm.
//!
//! ## Design principles
//!
//! - **No live GitHub calls or LLM inputs.** Every input to frame
//!   construction and draw selection comes from persisted facts in
//!   `djinn-db` or from the operator-supplied sample policy.
//! - **Immutable frames.** Once sealed, a frame revision is never mutated.
//!   Late corrections (e.g. a stratum change for a merged change after
//!   seal) produce a new revision with a typed audit event.
//! - **Deterministic replay.** The `hmac-sha256-counter-v1` algorithm
//!   uses only the sealed frame content and a revealed seed — any
//!   verifier with the same inputs derives identical selections.
//!
//! ## Algorithm: `hmac-sha256-counter-v1`
//!
//! For each eligible merged-change id in a stratum, compute:
//!
//! ```text
//! score(i) = HMAC-SHA256(key=seed, message=stratum_name || ":" || counter_i)
//! ```
//!
//! where `counter_i` is the zero-based index of the id in the
//! canonical sorted list. The scores are interpreted as big-endian
//! 256-bit unsigned integers. The top-k items (by lowest score wins,
//! i.e. lexicographically smallest HMAC) are selected, where k is
//! determined by `ceil(eligible_count * stratum_rate)`.
//!
//! This ensures:
//! - Determinism: same seed + same frame → same draw
//! - Independence: different strata use different stratum_name prefixes
//! - Verifiability: replay helpers recompute scores from stored frame content

pub mod frame;
pub mod random;

#[cfg(test)]
mod tests;

// Re-exports for sibling modules and external callers.
pub use frame::{
    ExclusionReason, FrameBuilder, FrameBuilderError, SamplePolicy, SealedFrame,
    SealedFrameEventPayload, StratumFrame,
};
pub use random::{
    DrawResult, DrawnItem, HMAC_SHA256_COUNTER_V1, ReplayVerification, SeedCommitment, commit_seed,
    draw_selections, verify_replay,
};
