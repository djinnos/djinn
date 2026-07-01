# Slot Facade Inventory — `djinn_agent::actors::slot`

> **Task:** hw3r — Inventory and document djinn-agent slot facade compatibility exports
> **Epic:** p6i4 — Slot cut-over host facade: remove djinn-agent duplicate behavior while preserving callers
> **Generated:** 2026-07-01

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
├── llm_extraction.rs         ← THIN SHIM: re-exports run_llm_extraction from djinn-slot,
│                                 agent-side test helpers retained
├── memory_enrichment.rs      ← EMPTY SHIM: mod file exists; types/re-exports are in mod.rs
├── pool/                     ← HOST-ONLY: SlotPoolHandle, SlotFactory, PoolStatus, etc.
├── reply_loop/               ← HOST-ONLY: AgentContext-based reply loop wiring
│   ├── mod.rs                ← re-exports ReplyLoopContext, run_reply_loop
│   ├── turn.rs               ← actual loop implementation (host-specific)
│   ├── streaming.rs
│   ├── tool_dispatch.rs
│   ├── error_handling.rs
│   ├── loop_guard.rs
│   ├── budget.rs
│   └── persistence.rs
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
| `run_llm_extraction` | `djinn_slot::run_llm_extraction` | No (facade only) |
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
| `server/src/mcp_bridge/memory_enrichment.rs` | `djinn_agent::actors::slot` | `run_memory_enrichment_with_db`, `EnrichmentReport`, `EnrichmentEntity`, `EnrichmentClaim`, `EnrichmentEdge` | **Migration candidate** → `djinn_slot::*` |
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
| `actors/slot/reply_loop/*.rs` | `crate::actors::slot::{finalize_handlers, helpers}` | Host-only |
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
- **`reply_loop/`** — Agent-specific reply loop wiring (uses `AgentContext`
  directly)
- **`pool/`** — `SlotPoolHandle` and pool actor (host-specific wiring)
- **`actor.rs`** — `SlotActor` (uses `AgentContext` directly)
- **`helpers/`** — Provider resolution, feedback, code context (pub + pub(crate))

---

## Migration Candidates (later tasks)

These external callers should eventually migrate to direct `djinn_slot::*`
imports. The facade preserves them until migration tasks run:

1. **`server/src/mcp_bridge/memory_enrichment.rs`** — All enrichment types and
   `run_memory_enrichment_with_db` are canonical `djinn_slot` exports.
   Migrate to `djinn_slot::{EnrichmentReport, ...}` when the dependency
   graph allows it.

2. **`server/crates/djinn-agent-worker/src/worker_services.rs`** — Helper
   functions (`parse_model_id`, `default_base_url`, etc.) are host-only
   but could use `djinn_slot::helpers::*` directly if the agent-worker
   adds a `djinn-slot` dependency.

---

## Build Verification

After facade inventory changes, the following must compile:

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
