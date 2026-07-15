# Slot Facade Inventory — `djinn_agent::actors::slot`

> **Task:** hw3r — Inventory and document djinn-agent slot facade compatibility exports
> **Epic:** p6i4 — Slot cut-over host facade: remove djinn-agent duplicate behavior while preserving callers
> **Generated:** 2026-07-01  
> **Updated:** 2026-07-01 — p6i4 task bohx: final host-dispatch facade cleanup and duplicate-file proof

## Overview

`server/crates/djinn-agent/src/actors/slot/mod.rs` serves as a **compatibility facade**
after the canonical slot implementation was extracted to `djinn-slot` (epic lvft).
The facade's job is to:

1. **Re-export** canonical types from `djinn_slot` so that existing
   `djinn_agent::actors::slot::*` import paths continue to resolve.
2. **Retain host-only modules** that wire `AgentContext` into the canonical
   slot APIs (host callbacks, lifecycle stage helpers, dispatch glue).
3. **Provide thin adapter wrappers** (`AgentContext` → `SlotContext`) for
   agent-internal callers that haven't yet migrated to direct `djinn_slot::*`.

---

## Module Structure

```
djinn-agent/src/actors/slot/
├── mod.rs                    ← facade: re-exports + submodule declarations
├── actor.rs                  ← HOST-ONLY: SlotActor, SlotHandle (lifecycle runner)
├── commands.rs               ← THIN SHIM: re-exports SlotCommand/SlotError from djinn-slot,
│                                 adapter for log_commands_run_event
├── finalize_handlers.rs      ← THIN SHIM: re-exports apply_ac_verdicts from djinn-slot,
│                                 adapters for process_finalize_payload/handle_budget_park
├── helpers/
│   ├── mod.rs                ← HOST-ONLY: re-exports provider_resolution (pub) +
│   │                           feedback/code_context (pub(crate))
│   ├── provider_resolution   ← HOST-ONLY: ProviderCredential, auth_method_for_provider, etc.
│   ├── feedback.rs           ← HOST-ONLY (pub(crate)): conflict_context, format_command_details, etc.
│   └── code_context.rs       ← HOST-ONLY (pub(crate)): build_role_code_graph_context, etc.
├── host_callbacks.rs         ← HOST-ONLY: AgentDispatchCallbacks + agent_to_dispatch_slot_context
├── lifecycle/                ← HOST-ONLY: per-stage helpers used by supervisor_impl/stage.rs
│   ├── mcp_resolve.rs
│   ├── model_resolution.rs
│   ├── prompt_context.rs
│   ├── retry.rs
│   ├── role_overrides.rs
│   ├── setup.rs
│   ├── task_classifier.rs
│   └── teardown.rs
├── llm_extraction.rs         ← THIN SHIM: AgentContext→SlotContext test adapters;
│                                 production behavior removed in favor of djinn-slot
├── memory_enrichment.rs      ← EMPTY SHIM: mod file exists; types/re-exports are in mod.rs
├── pool/                     ← HOST-ONLY: SlotPoolHandle, SlotFactory, PoolStatus, etc.
├── reply_loop/               ← THIN SHIM: adapts AgentContext into canonical
│   │                           djinn-slot reply-loop API
│   ├── mod.rs                ← ReplyLoopContext, AgentToolDispatcher, run_reply_loop adapter;
│   │                           error_handling + loop_guard re-exported from djinn-slot
├── session_extraction.rs     ← THIN SHIM: AgentContext→SlotContext adapter,
│                                 delegates to djinn_slot::run_extraction_backfill
└── supervisor_runner.rs      ← HOST-ONLY: dispatch_task_runtime (host-side dispatch logic)
```

---

## Re-exports from `djinn_slot` (facade compatibility)

These symbols are re-exported in `mod.rs` so that existing
`djinn_agent::actors::slot::*` import paths continue to resolve:

| Symbol | Canonical source | Used by external callers? |
|--------|-----------------|--------------------------|
| `SlotEvent` | `djinn_slot::SlotEvent` | Yes (control-plane, server) |
| `EnrichmentClaim` | `djinn_slot::EnrichmentClaim` | Yes (mcp_bridge) |
| `EnrichmentEdge` | `djinn_slot::EnrichmentEdge` | Yes (mcp_bridge) |
| `EnrichmentEntity` | `djinn_slot::EnrichmentEntity` | Yes (mcp_bridge) |
| `EnrichmentReport` | `djinn_slot::EnrichmentReport` | Yes (mcp_bridge) |
| `run_memory_enrichment` | `djinn_slot::run_memory_enrichment` | No (facade only) |
| `run_memory_enrichment_with_db` | `djinn_slot::run_memory_enrichment_with_db` | Yes (mcp_bridge) |
| `run_llm_extraction` | `djinn_slot::run_llm_extraction` | No (facade only; module-level agent adapter kept for tests) |
| `SlotCommand` | `djinn_slot::SlotCommand` | Yes (control-plane) |
| `SlotError` | `djinn_slot::SlotError` | Yes (control-plane) |
| `apply_ac_verdicts` | `djinn_slot::finalize_handlers` | No (pub(crate)) |

Re-exported from `djinn_orchestration_types` (shared DTOs, not `djinn_slot`):

| Symbol | Source |
|--------|--------|
| `MERGE_CONFLICT_PREFIX` | `djinn_orchestration_types::slot` |
| `MergeConflictMetadata` | `djinn_orchestration_types::slot` |
| `ModelSlotConfig` | `djinn_orchestration_types::slot` |
| `SlotInfo` | `djinn_orchestration_types::slot` |
| `SlotPoolConfig` | `djinn_orchestration_types::slot` |
| `SlotState` | `djinn_orchestration_types::slot` |

Re-exported via `pub use actor::*`:

| Symbol | Notes |
|--------|-------|
| `SlotActor` | Host-only actor implementation |
| `SlotHandle` | Public handle for spawning slot actors |
| `TestLifecycleRunner` | Test-support type (cfg-gated) |

Re-exported via `pub use pool::*`:

| Symbol | Notes |
|--------|-------|
| `SlotPoolHandle` | Public pool handle used by server/coordinator |
| `ModelPoolStatus` | Pool status type |
| `PoolError` | Pool error type |
| `PoolMessage` | Pool message type |
| `PoolStatus` | Pool status type |
| `RunningTaskInfo` | Running task info type |
| `SlotFactory` | Test-support type (cfg-gated) |

Re-exported via `pub use helpers::*`:

| Symbol | Notes |
|--------|-------|
| `OAuthAuthMethodWire` | Wire format for OAuth auth method |
| `OAuthCapabilitiesWire` | Wire format for OAuth capabilities |
| `OAuthConfigWire` | Wire format for OAuth config |
| `OAuthFormatFamilyWire` | Wire format for format family |
| `ProviderCredential` | Credential type |
| `auth_method_for_provider` | Provider auth method helper |
| `capabilities_for_provider` | Provider capabilities helper |
| `default_base_url` | Provider base URL helper |
| `format_family_for_provider` | Provider format family helper |
| `load_provider_credential` | Credential loading helper |
| `parse_model_id` | Model ID parsing helper |
| `refresh_oauth_credential_after_401` | OAuth refresh helper |

---

## Caller Scan

### External callers (outside `djinn-agent`)

| Caller file | Import path | Symbols used | Category |
|-------------|-------------|--------------|----------|
| `server/src/server/state/mod.rs` | `djinn_agent::actors::slot` | `SlotPoolConfig`, `SlotPoolHandle` | **Preserved facade** |
| `server/src/server/state/settings.rs` | `djinn_agent::actors::slot` | `ModelSlotConfig`, `SlotPoolConfig` | **Preserved facade** |
| `server/src/server/chat/handler.rs` | `djinn_agent::actors::slot` | `ProviderCredential`, `auth_method_for_provider`, `capabilities_for_provider`, `default_base_url`, `format_family_for_provider`, `load_provider_credential`, `parse_model_id` | **Preserved facade** (host-only helpers) |
| `server/src/server/chat/prompt/system_message.rs` | `djinn_agent::actors::slot` | `format_family_for_provider`, `parse_model_id` | **Preserved facade** (host-only helpers) |
| `server/src/server/tests/debug.rs` | `djinn_agent::actors::slot` | `SlotPoolConfig`, `SlotPoolHandle` | **Preserved facade** |
| `server/src/mcp_bridge/bridges.rs` | `djinn_agent::actors::slot` | `SlotPoolHandle` | **Preserved facade** |
| `server/src/mcp_bridge/memory_enrichment.rs` | `djinn_slot` | `run_memory_enrichment_with_db`, `EnrichmentReport`, `EnrichmentEntity`, `EnrichmentClaim`, `EnrichmentEdge` | **Migrated by p6i4 slice `019f1ad9`** |
| `server/crates/djinn-control-plane/tests/execution_tools.rs` | `djinn_agent::actors::slot` | `ModelSlotConfig`, `SlotFactory`, `SlotHandle`, `SlotPoolConfig`, `SlotPoolHandle` | **Preserved facade** |
| `server/crates/djinn-agent-worker/src/worker_services.rs` | `djinn_agent::actors::slot::helpers` | `OAuthConfigWire`, `auth_method_for_provider`, `capabilities_for_provider`, `default_base_url`, `format_family_for_provider`, `parse_model_id` | **Migration candidate** → `djinn_agent::actors::slot::helpers` or `djinn_slot::helpers` |

### Internal callers (within `djinn-agent`)

| Caller file | Import path | Category |
|-------------|-------------|----------|
| `roles/worker.rs` | `crate::actors::slot::helpers::initial_user_message_for_task` | Host-only |
| `lib.rs` | `actors::slot::session_extraction::run_extraction_backfill` | Host-only (re-exported at crate root) |
| `supervisor_impl/stage.rs` | `crate::actors::slot::{helpers, lifecycle, reply_loop, finalize_handlers}` | Host-only |
| `supervisor_impl/pr.rs` | `crate::actors::slot::helpers::default_target_branch` | Host-only |
| `extension/handlers.rs` | `crate::actors::slot::lifecycle::task_classifier` | Host-only |
| `direct_services.rs` | `crate::actors::slot::{helpers, lifecycle}` | Host-only |
| `prompts/tests/visual_spec.rs` | `crate::actors::slot::lifecycle::{task_classifier, mcp_resolve}` | Host-only (test) |
| `actors/coordinator/mod.rs` | `crate::actors::slot::SlotPoolHandle` | Host-only |
| `actors/slot/lifecycle/*.rs` | `crate::actors::slot::{helpers, MergeConflictMetadata, commands}` | Host-only |
| `actors/slot/llm_extraction*.rs` | `crate::actors::slot::{helpers, lifecycle, session_extraction}` | Host-only |
| `actors/slot/reply_loop/mod.rs` | `crate::actors::slot::{host_callbacks, output_stash, extension}` | Host-only (thin adapter) |
| `actors/slot/supervisor_runner.rs` | `crate::actors::slot::{lifecycle, session_extraction, helpers}` | Host-only |

---

## Modules Intentionally Remaining in Agent

The following modules are **host-only** — they contain logic specific to
`AgentContext` wiring, host-side dispatch, or stage-specific helpers that
depend on `AgentContext` fields. They are **not** candidates for extraction
to `djinn-slot`:

- **`host_callbacks.rs`** — `AgentDispatchCallbacks` implementing
  `SlotHostCallbacks` for the dispatch pathway
- **`lifecycle/`** — Per-stage helpers (`setup`, `model_resolution`,
  `mcp_resolve`, `prompt_context`, `role_overrides`, `task_classifier`,
  `teardown`, `retry`) used by `supervisor_impl/stage.rs`
- **`supervisor_runner.rs`** — `dispatch_task_runtime` host-side dispatch
- **`reply_loop/`** — Agent-context reply loop adapter: wraps `AgentContext` into
  `SlotContext` + `AgentToolDispatcher` and delegates to `djinn_slot::reply_loop::*`
- **`pool/`** — `SlotPoolHandle` and pool actor (host-specific wiring)
- **`actor.rs`** — `SlotActor` (uses `AgentContext` directly)
- **`helpers/`** — Provider resolution, feedback, code context (pub + pub(crate))

---

## Migration Candidates (later tasks)

These external callers should eventually migrate to direct `djinn_slot::*`
imports. The facade preserves them until migration tasks run:

1. **`server/crates/djinn-agent-worker/src/worker_services.rs`** — Helper
   functions (`parse_model_id`, `default_base_url`, etc.) are host-only
   but could use `djinn_slot::helpers::*` directly if the agent-worker
   adds a `djinn-slot` dependency.

### Caller migrations completed by p6i4 slice `019f1ad9`

- **`server/src/mcp_bridge/memory_enrichment.rs`** now imports and converts
  canonical `djinn_slot::{EnrichmentReport, EnrichmentEntity,
  EnrichmentClaim, EnrichmentEdge}` values and calls
  `djinn_slot::run_memory_enrichment_with_db` directly. The root
  `djinn-server` crate therefore has an explicit `djinn-slot` dependency.

---

## Build Verification

After facade inventory/cut-over changes, the following must compile:

```bash
cd server/
cargo build -p djinn-agent --all-features
cargo build -p djinn-slot --all-features
```

---

## Notes for Deletion Tasks (epic p6i4 follow-up)

When later tasks delete duplicate implementation files from `djinn-agent`,
they must:

1. **Consult this inventory** to verify which modules are thin shims (safe to
   delete or replace with pure re-exports) vs. host-only (must remain).
2. **Run the caller scan** again (`grep -R "actors::slot" server/`) to catch
   any callers introduced since this inventory was generated.
3. **Update this document** to reflect the new module state after each
   deletion wave.

---

## p6i4 slice `019f1ad9` shim inventory update

This slice thinned the extraction/enrichment-facing agent modules as follows:

| Agent file | Current status | Canonical owner |
|------------|----------------|-----------------|
| `commands.rs` | Thin compatibility wrapper: re-exports `SlotCommand`/`SlotError`; adapts `AgentContext` for `log_commands_run_event`. | `djinn_slot::commands` |
| `finalize_handlers.rs` | Thin compatibility wrapper: adapts `AgentContext` for `process_finalize_payload`/`handle_budget_park`; re-exports canonical test helper/types. | `djinn_slot::finalize_handlers` / `djinn_slot::finalize_types` |
| `session_extraction.rs` | Thin compatibility wrapper: owns only `AgentContext`→`SlotContext` conversion/no-op callback glue plus adapters for backfill/post-session extraction and test structural extraction. | `djinn_slot::session_extraction` |
| `llm_extraction.rs` | Thin compatibility wrapper: the former 2k+ line duplicate implementation was removed; remaining code is test-only `AgentContext`→`SlotContext` adapters around `djinn_slot::llm_extraction` entry points. | `djinn_slot::llm_extraction` |
| `memory_enrichment.rs` | Empty compatibility module retained only so `mod memory_enrichment;` resolves; public surface is re-exported from `djinn_slot` in `mod.rs`. | `djinn_slot::memory_enrichment` |

For the Phase 1 terminal-context boundary and the deferred merge-queue verdict
path, see
[`server/docs/knowledge-extraction/merge-queue-verdict.md`](../../../docs/knowledge-extraction/merge-queue-verdict.md).

No independent extraction, enrichment, prompt, deduplication, admission-gate,
finalization, or command-activity behavior remains in these agent files.

---

## p6i4 task bohx — Final host-dispatch facade cleanup and duplicate-file proof

**Task:** bohx — Finish slot host-dispatch facade cleanup and final duplicate-file proof
**Date:** 2026-07-01

### Files deleted (dead duplicate code)

| Deleted file | Lines | Reason |
|---|---|---|
| `reply_loop/turn.rs` | 2,227 | Not declared in `reply_loop/mod.rs`; canonical implementation in `djinn_slot::reply_loop::turn`. Dead file — unreachable from the module graph. |
| `reply_loop/durable_progress/mod.rs` | 634 | Not declared in `reply_loop/mod.rs`; only referenced from dead `turn.rs`. Dead directory — unreachable from the module graph. |

**Total dead code removed:** 2,861 lines.

### Files confirmed as host-only (retained intentionally)

| File | Lines | Category | Rationale |
|---|---|---|---|
| `supervisor_runner.rs` | 1,755 | HOST-ONLY | Contains the actual host-side dispatch logic (`dispatch_task_runtime`) that resolves tasks, builds `TaskRunSpec`, drives K8s/Test runtimes, handles provider failover, and persists loop-guard activity. djinn-slot's `supervisor_runner.rs` (29 lines) is a thin delegation to host callbacks. This file IS the host callback implementation. |
| `host_callbacks.rs` | 185 | HOST-ONLY | `AgentDispatchCallbacks` implementing `SlotHostCallbacks`; bridges `AgentContext` → `SlotContext` for the dispatch pathway. |
| `lifecycle/*.rs` | ~various | HOST-ONLY | Per-stage helpers used by `supervisor_impl/stage.rs`, depend on `AgentContext`. |
| `actor.rs` | 125 | HOST-ONLY | `SlotHandle` compatibility wrapper that adapts `AgentContext` at spawn time. |
| `pool/` | ~200 | HOST-ONLY | `SlotPoolHandle` wrapper, `SlotFactory` test type. |
| `helpers/` | ~various | HOST-ONLY | Provider resolution, feedback, code context. |

### Files confirmed as thin shims (retained for compatibility)

| File | Lines | Category | Notes |
|---|---|---|---|
| `commands.rs` | 32 | THIN SHIM | Re-exports `SlotCommand`/`SlotError`; thin `log_commands_run_event` adapter. |
| `finalize_handlers.rs` | 477 | THIN SHIM | Adapters + test coverage for `process_finalize_payload`/`handle_budget_park`. |
| `session_extraction.rs` | 215 | THIN SHIM | `agent_to_slot_context` + backfill/post-session adapters. |
| `llm_extraction.rs` | 65 | THIN SHIM | Test-only adapters (all `#[cfg(test)]`). |
| `memory_enrichment.rs` | 25 | EMPTY SHIM | Module file only; re-exports in `mod.rs`. |
| `reply_loop/mod.rs` | 243 | THIN SHIM | `AgentToolDispatcher` adapter + `run_reply_loop` wrapper; re-exports `error_handling`/`loop_guard` from djinn-slot. |

### Final module tree

```
djinn-agent/src/actors/slot/
├── mod.rs                    (175 lines) — facade: re-exports + submodule declarations
├── actor.rs                  (125 lines) — HOST-ONLY: SlotHandle wrapper
├── commands.rs               (32 lines)  — THIN SHIM
├── finalize_handlers.rs      (477 lines) — THIN SHIM + tests
├── helpers/
│   ├── mod.rs                — HOST-ONLY: pub re-exports
│   ├── provider_resolution   — HOST-ONLY
│   ├── feedback.rs           — HOST-ONLY (pub(crate))
│   ├── code_context.rs       — HOST-ONLY (pub(crate))
│   ├── reviewer_diff.rs      — HOST-ONLY (pub(crate))
│   └── tests.rs              — test coverage
├── host_callbacks.rs         (185 lines) — HOST-ONLY
├── lifecycle/
│   ├── mcp_resolve.rs        — HOST-ONLY
│   ├── model_resolution.rs   — HOST-ONLY
│   ├── prompt_context.rs     — HOST-ONLY
│   ├── prompt_context_tests.rs — test coverage
│   ├── ci_directive_tests.rs — test coverage
│   ├── retry.rs              — HOST-ONLY
│   ├── role_overrides.rs     — HOST-ONLY
│   ├── setup.rs              — HOST-ONLY
│   ├── task_classifier.rs    — HOST-ONLY
│   └── teardown.rs           — HOST-ONLY
├── llm_extraction.rs         (65 lines)  — THIN SHIM (test-only)
├── llm_extraction_tests.rs   — test coverage
├── memory_enrichment.rs      (25 lines)  — EMPTY SHIM
├── pool/
│   ├── mod.rs                — HOST-ONLY: re-exports
│   ├── handle.rs             — HOST-ONLY: SlotPoolHandle wrapper
│   └── types.rs              — HOST-ONLY: re-exports + SlotFactory
├── reply_loop/
│   └── mod.rs                (243 lines) — THIN SHIM: adapter + re-exports
├── session_extraction.rs     (215 lines) — THIN SHIM
├── supervisor_runner.rs      (1,755 lines) — HOST-ONLY: dispatch_task_runtime
├── helpers_tests.rs          — test coverage
```

### Duplicate-proof summary

Every file remaining under `djinn-agent/src/actors/slot/` is one of:
1. **Host-only** — contains agent-specific dispatch/callback/lifecycle logic that depends on `AgentContext` and has no duplicate in `djinn-slot`
2. **Thin shim** — re-exports canonical types from `djinn-slot` and provides `AgentContext` → `SlotContext` adapters for backward compatibility
3. **Test coverage** — tests for host-only or thin-shim code

No file contains an independent copy of logic that exists in `djinn-slot`. The facade `mod.rs` re-exports canonical types (`SlotEvent`, enrichment types, `SlotCommand`, `SlotError`, etc.) so existing `djinn_agent::actors::slot::*` import paths continue to resolve.
