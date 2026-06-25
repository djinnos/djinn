// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
// ─── hfhw cutover: memory enrichment delegated to djinn-slot ───────────────
//
// The full memory enrichment implementation (types, LLM prompt, batch loop,
// entity dedup, edge persistence, `run_memory_enrichment_inner`) now lives
// in `djinn-slot::memory_enrichment`.
//
// This module is a thin compatibility shim.  The public types and entry
// points are re-exported from `djinn-slot` via `super::mod.rs`:
//
//   pub use djinn_slot::{
//       EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport,
//       run_memory_enrichment, run_memory_enrichment_with_db,
//   };
//
// The old production implementation of `run_memory_enrichment_inner` has been
// removed from `djinn-agent`; it is no longer production-reachable.
//
// This file is intentionally minimal.  Any agent-internal callers that
// previously used `super::memory_enrichment::run_memory_enrichment_from_context`
// should switch to the public `run_memory_enrichment_with_db` API.

// No production code remains here.  The module file is retained so that
// `mod memory_enrichment;` in `super::mod.rs` resolves; the public surface
// is provided by the re-exports in `super::mod.rs`.
