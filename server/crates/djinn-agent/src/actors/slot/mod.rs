// ─── Slot Facade (p6i4 compatibility layer) ──────────────────────────────
//
// This module is the **compatibility facade** after canonical slot
// implementation was extracted to `djinn-slot` (epic lvft).  It serves three
// purposes:
//
// 1. **Re-export canonical types** from `djinn_slot` and
//    `djinn_orchestration_types` so existing `djinn_agent::actors::slot::*`
//    import paths continue to resolve for workspace callers.
//
// 2. **Retain host-only modules** that wire `AgentContext` into the canonical
//    slot APIs: host callbacks, lifecycle stage helpers, dispatch glue,
//    pool management, and the agent-side reply loop.
//
// 3. **Provide thin adapter wrappers** (`AgentContext` → `SlotContext`) for
//    agent-internal callers and external callers that haven't yet migrated
//    to direct `djinn_slot::*` imports.
//
// ┌──────────────────────────────────────────────────────────────────────┐
// │ Module categories:                                                   │
// │                                                                      │
// │ PRESERVED FACADE (re-exports) — symbols below whose canonical         │
// │   source is `djinn_slot` or `djinn_orchestration_types`.  These are   │
// │   the compatibility contract: removing them would break external      │
// │   callers.  Migration candidates are noted in the docs.              │
// │                                                                      │
// │ THIN SHIM — modules whose production logic was removed and replaced  │
// │   with `AgentContext → SlotContext` adapters that delegate to         │
// │   `djinn_slot`.  Retained for compile compatibility; safe to replace  │
// │   with pure re-exports in later deletion waves.                      │
// │                                                                      │
// │ HOST-ONLY — modules containing agent-specific logic that depends on   │
// │   `AgentContext` fields and is NOT duplicated in `djinn-slot`.        │
// │   These must remain in the agent crate.                              │
// └──────────────────────────────────────────────────────────────────────┘
//
// Full inventory: `docs/slot-facade-inventory.md`

// ═══════════════════════════════════════════════════════════════════════════
// PRESERVED FACADE: Re-exports from djinn-orchestration-types
// ═══════════════════════════════════════════════════════════════════════════
// Shared slot DTOs and config types owned by the orchestration-types crate.
// Used by: server state, control-plane tests, agent-worker, coordinator.

pub use djinn_orchestration_types::slot::{
    MERGE_CONFLICT_PREFIX, MergeConflictMetadata, ModelSlotConfig, SlotInfo, SlotPoolConfig,
    SlotState,
};

// ═══════════════════════════════════════════════════════════════════════════
// PRESERVED FACADE: SlotEvent (canonical re-export from djinn-slot)
// ═══════════════════════════════════════════════════════════════════════════
// The canonical `SlotEvent` enum lives in `djinn-slot`.  Re-export so
// `djinn_agent::actors::slot::SlotEvent` and `djinn_slot::SlotEvent` name
// the same type (not a duplicate).
// Used by: control-plane, server, agent-internal actor/pool/coordinator.

pub use djinn_slot::SlotEvent;

// ═══════════════════════════════════════════════════════════════════════════
// PRESERVED FACADE: Memory enrichment re-exports (canonical: djinn-slot)
// ═══════════════════════════════════════════════════════════════════════════
// Types and entry points now owned by `djinn-slot`.  Re-exported so external
// callers (`djinn-server`'s `mcp_bridge`, `djinn-control-plane` test-support)
// continue to compile under `djinn_agent::actors::slot::*` paths.
// MIGRATION CANDIDATE: `server/src/mcp_bridge/memory_enrichment.rs` could
// migrate to `djinn_slot::*` when the dependency graph allows it.

pub use djinn_slot::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, run_memory_enrichment,
    run_memory_enrichment_with_db,
};

// ═══════════════════════════════════════════════════════════════════════════
// PRESERVED FACADE: LLM extraction re-export (canonical: djinn-slot)
// ═══════════════════════════════════════════════════════════════════════════
// The canonical `run_llm_extraction` entry point now lives in `djinn-slot`.
// Keep a root-level facade export so downstream cleanup can switch callers
// away from the duplicate agent module without losing the historic path.

pub use djinn_slot::run_llm_extraction;

// ═══════════════════════════════════════════════════════════════════════════
// Submodules (host-only, thin shims, and test modules)
// ═══════════════════════════════════════════════════════════════════════════

// HOST-ONLY: SlotActor lifecycle runner and SlotHandle.
// Uses AgentContext directly for dispatch; not duplicated in djinn-slot.
mod actor;

// THIN SHIM: SlotCommand/SlotError re-exported from djinn-slot.
// Provides agent-compatible wrapper for `log_commands_run_event`.
mod commands;

// THIN SHIM: Finalize handler types re-exported from djinn-slot.
// Provides AgentContext → SlotContext adapters for process_finalize_payload
// and handle_budget_park. `apply_ac_verdicts` is re-exported directly.
pub(crate) mod finalize_handlers;

// HOST-ONLY: Provider resolution, feedback, and code-context helpers.
// `pub` surface: ProviderCredential, auth_method_for_provider,
// capabilities_for_provider, default_base_url, format_family_for_provider,
// load_provider_credential, parse_model_id, OAuthConfigWire, etc.
// `pub(crate)` surface: build_role_code_graph_context,
// conflict_context_for_dispatch, extract_worker_context, format_command_details,
// initial_user_message_for_task, load_task, etc.
// Used by: server chat handler, system_message, agent-worker, supervisor_impl.
pub mod helpers;

// HOST-ONLY: AgentDispatchCallbacks implementing SlotHostCallbacks for the
// dispatch pathway. Dispatches through supervisor_runner::dispatch_task_runtime.
// Not duplicated in djinn-slot.
pub(crate) mod host_callbacks;

// HOST-ONLY: Per-stage lifecycle helpers (setup / model resolution / mcp
// resolve / prompt context / role overrides / task classifier / teardown /
// retry) used by supervisor_impl/stage.rs.  Depends on AgentContext.
// Not duplicated in djinn-slot.
pub(crate) mod lifecycle;

// THIN SHIM: `run_llm_extraction` is re-exported at facade level above.
// This module retains agent-side test helpers and LLM extraction adapters
// that depend on AgentContext/provider resolution.
pub(crate) mod llm_extraction;

// EMPTY SHIM: Module file retained for `mod memory_enrichment;` to resolve.
// All production types and entry points are re-exported from djinn-slot
// via the facade re-exports above. No production code remains here.
mod memory_enrichment;

// HOST-ONLY: SlotPoolHandle, SlotFactory (test-support), pool actor, and
// pool status types. Host-specific wiring to AgentContext.
mod pool;

// HOST-ONLY: Agent-specific reply loop wiring (uses AgentContext directly).
// Contains turn, streaming, tool_dispatch, error_handling, loop_guard,
// budget, and persistence submodules. Not a shim — contains real logic.
pub(crate) mod reply_loop;

#[cfg(test)]
mod reply_loop_tests;

// THIN SHIM: `run_extraction_backfill` adapter that converts
// AgentContext → SlotContext and delegates to `djinn_slot::run_extraction_backfill`.
// The full structural extraction implementation now lives in djinn-slot.
// Re-exports: ExtractionQuality, SessionTaxonomy, derive_scope_paths.
pub(crate) mod session_extraction;

// HOST-ONLY: `dispatch_task_runtime` host-side dispatch logic, called through
// `host_callbacks::AgentDispatchCallbacks`.  Not duplicated in djinn-slot.
mod supervisor_runner;

// ═══════════════════════════════════════════════════════════════════════════
// PRESERVED FACADE: Wildcard re-exports (actor, pool, helpers)
// ═══════════════════════════════════════════════════════════════════════════
// These `pub use *` re-exports preserve the public surface that external
// callers depend on.  Key symbols: SlotActor, SlotHandle, TestLifecycleRunner
// (from actor); SlotPoolHandle, ModelPoolStatus, PoolError, PoolMessage,
// PoolStatus, RunningTaskInfo, SlotFactory (from pool); ProviderCredential,
// auth_method_for_provider, capabilities_for_provider, default_base_url,
// format_family_for_provider, load_provider_credential, parse_model_id,
// OAuthConfigWire, refresh_oauth_credential_after_401 (from helpers).
// MIGRATION CANDIDATE: `djinn-agent-worker/src/worker_services.rs` could
// migrate helper imports to `djinn_slot::helpers::*` when available.

pub use actor::*;
pub(crate) use commands::*;
pub use helpers::*;
pub use pool::*;

// ═══════════════════════════════════════════════════════════════════════════
// Test modules
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod helpers_tests;

#[cfg(test)]
mod llm_extraction_tests;
