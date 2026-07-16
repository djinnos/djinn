# Slot Cut-Over Baseline Inventory

> Epic: `aaiz` — Slot cut-over foundation: baseline inventory and reconciliation plan  
> Proposal: `flpe` — Finish the djinn-slot extraction cut-over (de-duplicate the slot subsystem)  
> Artifact: `docs/slot-cutover/baseline-inventory.md`  
> Scope: documentation-only; no behavior, API, or visibility changes.

---

## 1. Baseline line counts

Commands run from the repository root on the current branch (exact output preserved):

```bash
$ find server/crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
24490

$ find server/crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
20822

$ find server/crates/djinn-agent/src/actors/slot server/crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
45312
```

| Tree | Lines | Notes |
|------|-------|-------|
| `server/crates/djinn-agent/src/actors/slot` | **24,490** | Original slot subsystem still embedded in djinn-agent. |
| `server/crates/djinn-slot/src` | **20,822** | Extracted crate; some modules are duplicates, some are canonical. |
| **Combined** | **45,312** | Pre-cut-over total; final cut-over must prove a 15 k+ line reduction, not a relocation. |

---

## 2. File inventory

### 2.1 djinn-agent slot tree (`server/crates/djinn-agent/src/actors/slot`)

```
actor.rs
commands.rs
finalize_handlers.rs
helpers/code_context.rs
helpers/feedback.rs
helpers/mod.rs
helpers/provider_resolution.rs
helpers/reviewer_diff.rs
helpers/tests.rs
helpers_tests.rs
host_callbacks.rs
lifecycle.rs
lifecycle/mcp_resolve.rs
lifecycle/model_resolution.rs
lifecycle/prompt_context.rs
lifecycle/retry.rs
lifecycle/role_overrides.rs
lifecycle/setup.rs
lifecycle/task_classifier.rs
lifecycle/teardown.rs
llm_extraction.rs
llm_extraction_tests.rs
memory_enrichment.rs
mod.rs
pool/actor.rs
pool/handle.rs
pool/mod.rs
pool/tests.rs
pool/types.rs
reply_loop/budget.rs
reply_loop/error_handling.rs
reply_loop/loop_guard.rs
reply_loop/mod.rs
reply_loop/persistence.rs
reply_loop/streaming.rs
reply_loop/tests.rs
reply_loop/tool_dispatch.rs
reply_loop/turn.rs
reply_loop_tests.rs
session_extraction.rs
supervisor_runner.rs
```

### 2.2 djinn-slot crate (`server/crates/djinn-slot/src`)

```
actor.rs
commands.rs
finalize_handlers.rs
finalize_types.rs
helpers/code_context.rs
helpers/feedback.rs
helpers/mod.rs
helpers/provider_resolution.rs
helpers/reviewer_diff.rs
helpers/tests.rs
helpers_tests.rs
host.rs
lib.rs
lifecycle.rs
lifecycle/mcp_resolve.rs
lifecycle/model_resolution.rs
lifecycle/prompt_context.rs
lifecycle/retry.rs
lifecycle/role_overrides.rs
lifecycle/setup.rs
lifecycle/task_classifier.rs
lifecycle/teardown.rs
llm_extraction.rs
llm_extraction_tests.rs
memory_enrichment.rs
output_parser.rs
pool/actor.rs
pool/handle.rs
pool/mod.rs
pool/tests.rs
pool/types.rs
reply_loop/budget.rs
reply_loop/error_handling.rs
reply_loop/loop_guard.rs
reply_loop/mod.rs
reply_loop/persistence.rs
reply_loop/streaming.rs
reply_loop/tests.rs
reply_loop/tool_dispatch.rs
reply_loop/turn.rs
reply_loop_tests.rs
roles_support.rs
session_extraction.rs
supervisor_runner.rs
test_helpers.rs
truncate.rs
```

**Notable differences**
- `djinn-slot` adds: `finalize_types.rs`, `host.rs`, `output_parser.rs`, `roles_support.rs`, `test_helpers.rs`, `truncate.rs`.
- `djinn-agent` adds: `host_callbacks.rs` (agent-side host callback impl), `memory_enrichment.rs` (now a thin shim).
- Both trees contain near-identical copies of `commands.rs`, `llm_extraction.rs`, `actor.rs`, `supervisor_runner.rs`, lifecycle submodules, reply-loop submodules, and helper submodules.

---

## 3. Export/module surface inventory

### 3.1 `server/crates/djinn-agent/src/actors/slot/mod.rs`

#### Public re-exports (external callers may depend on these paths)

| Item | Path | Origin | Compatibility note |
|------|------|--------|-------------------|
| `MERGE_CONFLICT_PREFIX` | `djinn_agent::actors::slot::MERGE_CONFLICT_PREFIX` | `djinn_orchestration_types::slot` | Stable shared type. |
| `MergeConflictMetadata` | `djinn_agent::actors::slot::MergeConflictMetadata` | `djinn_orchestration_types::slot` | Stable shared type. |
| `ModelSlotConfig` | `djinn_agent::actors::slot::ModelSlotConfig` | `djinn_orchestration_types::slot` | Used by `server/src/server/state/settings.rs`, `server/src/server/tests/debug.rs`. |
| `SlotInfo` | `djinn_agent::actors::slot::SlotInfo` | `djinn_orchestration_types::slot` | Stable shared type. |
| `SlotPoolConfig` | `djinn_agent::actors::slot::SlotPoolConfig` | `djinn_orchestration_types::slot` | Used by `server/src/server/state/mod.rs`, `server/src/server/tests/debug.rs`, `server/src/server/state/settings.rs`. |
| `SlotState` | `djinn_agent::actors::slot::SlotState` | `djinn_orchestration_types::slot` | Stable shared type. |
| `SlotEvent` | `djinn_agent::actors::slot::SlotEvent` | `djinn_slot::SlotEvent` | Re-exported from canonical crate; also re-exported at `djinn_agent::SlotEvent` in `lib.rs`. |
| `EnrichmentClaim` | `djinn_agent::actors::slot::EnrichmentClaim` | `djinn_slot::memory_enrichment` | Re-exported from canonical crate. |
| `EnrichmentEdge` | `djinn_agent::actors::slot::EnrichmentEdge` | `djinn_slot::memory_enrichment` | Re-exported from canonical crate. |
| `EnrichmentEntity` | `djinn_agent::actors::slot::EnrichmentEntity` | `djinn_slot::memory_enrichment` | Re-exported from canonical crate. |
| `EnrichmentReport` | `djinn_agent::actors::slot::EnrichmentReport` | `djinn_slot::memory_enrichment` | Re-exported from canonical crate; used by `server/src/mcp_bridge/memory_enrichment.rs`. |
| `run_memory_enrichment` | `djinn_agent::actors::slot::run_memory_enrichment` | `djinn_slot::memory_enrichment` | Re-exported from canonical crate. |
| `run_memory_enrichment_with_db` | `djinn_agent::actors::slot::run_memory_enrichment_with_db` | `djinn_slot::memory_enrichment` | Re-exported from canonical crate; used by `server/src/mcp_bridge/memory_enrichment.rs`. |
| `actor::*` | `djinn_agent::actors::slot::*` (actor symbols) | `actor.rs` | Public re-export; includes `SlotActor`, `SlotHandle`, etc. |
| `helpers::*` | `djinn_agent::actors::slot::*` (helper symbols) | `helpers/mod.rs` | Public re-export; includes `format_family_for_provider`, `parse_model_id`, `initial_user_message_for_task`, etc. |
| `pool::*` | `djinn_agent::actors::slot::*` (pool symbols) | `pool/mod.rs` | Public re-export; includes `SlotPoolHandle`, `SlotPoolConfig`, `PoolError`, etc. |

#### `pub(crate)` / internal-only modules

| Module | Visibility | Notes |
|--------|------------|-------|
| `actor` | `mod` (private) | Re-exported via `pub use actor::*;` so selected symbols are public. |
| `commands` | `mod` (private) | Re-exported via `pub(crate) use commands::*;`. |
| `finalize_handlers` | `pub(crate) mod` | Used inside agent by `supervisor_impl/stage.rs`, `reply_loop/tests.rs`. |
| `helpers` | `pub mod` | Re-exported via `pub use helpers::*;`. |
| `host_callbacks` | `pub(crate) mod` | Agent-specific host callback glue (`AgentDispatchCallbacks`). Not intended for external callers. |
| `lifecycle` | `pub(crate) mod` | Deeply used by `supervisor_impl/stage.rs`, `direct_services.rs`, `prompts/tests/visual_spec.rs`, `llm_extraction.rs`. |
| `llm_extraction` | `pub(crate) mod` | Used by `direct_services.rs`, `llm_extraction_tests.rs`. |
| `pool` | `mod` (private) | Re-exported via `pub use pool::*;`. |
| `reply_loop` | `pub(crate) mod` | Used by `supervisor_impl/stage.rs`. |
| `reply_loop_tests` | `#[cfg(test)] mod` | Disabled in `djinn-slot`; still enabled here. |
| `session_extraction` | `pub(crate) mod` | Thin adapter delegating to `djinn_slot::session_extraction`. Re-exports `ExtractionQuality`, `SessionTaxonomy`, `derive_scope_paths`, `extract_session_signals`. |
| `supervisor_runner` | `mod` (private) | Host dispatch logic; called through `host_callbacks`. |
| `helpers_tests` | `#[cfg(test)] mod` | Test-only. |
| `llm_extraction_tests` | `#[cfg(test)] mod` | Test-only. |

### 3.2 `server/crates/djinn-slot/src/lib.rs`

#### Public re-exports (canonical crate surface)

| Item | Path | Origin | Compatibility note |
|------|------|--------|-------------------|
| `host` | `djinn_slot::host` | `host.rs` | Public module; contains `SlotContext`, `SlotHostCallbacks`, `KnowledgeBranchTarget`. |
| `SlotContext` | `djinn_slot::SlotContext` | `host.rs` | Also re-exported at `djinn_agent::SlotContext` in `lib.rs`. |
| `SlotHostCallbacks` | `djinn_slot::SlotHostCallbacks` | `host.rs` | Also re-exported at `djinn_agent::SlotHostCallbacks` in `lib.rs`. |
| `KnowledgeBranchTarget` | `djinn_slot::KnowledgeBranchTarget` | `host.rs` | Also re-exported at `djinn_agent::KnowledgeBranchTarget` in `lib.rs`. |
| `output_parser` | `djinn_slot::output_parser` | `output_parser.rs` | Public module. |
| `roles_support` | `djinn_slot::roles_support` | `roles_support.rs` | Public module. |
| `truncate` | `djinn_slot::truncate` | `truncate.rs` | Public module. |
| `MERGE_CONFLICT_PREFIX` | `djinn_slot::MERGE_CONFLICT_PREFIX` | `djinn_orchestration_types::slot` | Stable shared type. |
| `MergeConflictMetadata` | `djinn_slot::MergeConflictMetadata` | `djinn_orchestration_types::slot` | Stable shared type. |
| `ModelSlotConfig` | `djinn_slot::ModelSlotConfig` | `djinn_orchestration_types::slot` | Used by `djinn-coordinator` (many call sites). |
| `SlotInfo` | `djinn_slot::SlotInfo` | `djinn_orchestration_types::slot` | Stable shared type. |
| `SlotPoolConfig` | `djinn_slot::SlotPoolConfig` | `djinn_orchestration_types::slot` | Used by `djinn-coordinator` (many call sites). |
| `SlotState` | `djinn_slot::SlotState` | `djinn_orchestration_types::slot` | Stable shared type. |
| `actor::*` | `djinn_slot::*` (actor symbols) | `actor.rs` | Public re-export; includes `SlotActor`, `SlotHandle`, `TestLifecycleRunner`, etc. |
| `helpers::*` | `djinn_slot::*` (helper symbols) | `helpers/mod.rs` | Public re-export; same helper surface as agent. |
| `pool::*` | `djinn_slot::*` (pool symbols) | `pool/mod.rs` | Public re-export; includes `SlotPoolHandle`, `PoolError`, etc. |
| `EnrichmentClaim` | `djinn_slot::EnrichmentClaim` | `memory_enrichment.rs` | Also re-exported through agent facade. |
| `EnrichmentEdge` | `djinn_slot::EnrichmentEdge` | `memory_enrichment.rs` | Also re-exported through agent facade. |
| `EnrichmentEntity` | `djinn_slot::EnrichmentEntity` | `memory_enrichment.rs` | Also re-exported through agent facade. |
| `EnrichmentReport` | `djinn_slot::EnrichmentReport` | `memory_enrichment.rs` | Also re-exported through agent facade. |
| `run_memory_enrichment` | `djinn_slot::run_memory_enrichment` | `memory_enrichment.rs` | Also re-exported through agent facade. |
| `run_memory_enrichment_with_db` | `djinn_slot::run_memory_enrichment_with_db` | `memory_enrichment.rs` | Also re-exported through agent facade. |
| `ExtractionQuality` | `djinn_slot::ExtractionQuality` | `session_extraction.rs` | Also re-exported through agent facade. |
| `SessionTaxonomy` | `djinn_slot::SessionTaxonomy` | `session_extraction.rs` | Also re-exported through agent facade. |
| `SessionSignals` | `djinn_slot::SessionSignals` | `session_extraction.rs` | Also re-exported through agent facade. |
| `derive_scope_paths` | `djinn_slot::derive_scope_paths` | `session_extraction.rs` | Also re-exported through agent facade. |
| `extract_session_signals` | `djinn_slot::extract_session_signals` | `session_extraction.rs` | Also re-exported through agent facade (test-only in agent). |
| `run_extraction_backfill` | `djinn_slot::run_extraction_backfill` | `session_extraction.rs` | Also re-exported at `djinn_agent::run_extraction_backfill` in `lib.rs`. |
| `run_post_session_extraction` | `djinn_slot::run_post_session_extraction` | `session_extraction.rs` | — |
| `run_structural_extraction` | `djinn_slot::run_structural_extraction` | `session_extraction.rs` | — |
| `run_supervisor_dispatch` | `djinn_slot::run_supervisor_dispatch` | `supervisor_runner.rs` | Called by `djinn-agent::actor.rs` through host callback path. |
| `SlotEvent` | `djinn_slot::SlotEvent` | `lib.rs` (defined here) | Canonical definition; also re-exported at `djinn_agent::SlotEvent`. |
| `finalize_types` | `djinn_slot::finalize_types` | `finalize_types.rs` | Public module. |

#### `pub(crate)` / internal-only modules

| Module | Visibility | Notes |
|--------|------------|-------|
| `actor` | `mod` (private) | Re-exported via `pub use actor::*;`. |
| `commands` | `mod` (private) | Re-exported via `pub(crate) use commands::*;`. |
| `finalize_handlers` | `pub(crate) mod` | Same as agent copy. |
| `helpers` | `pub mod` | Re-exported via `pub use helpers::*;`. |
| `lifecycle` | `pub(crate) mod` | Same as agent copy. |
| `llm_extraction` | `pub(crate) mod` | Same as agent copy. |
| `memory_enrichment` | `pub(crate) mod` | Implementation; public items re-exported above. |
| `pool` | `mod` (private) | Re-exported via `pub use pool::*;`. |
| `reply_loop` | `pub(crate) mod` | Same as agent copy. |
| `session_extraction` | `pub(crate) mod` | Implementation; public items re-exported above. |
| `supervisor_runner` | `mod` (private) | Re-exported via `pub use supervisor_runner::run_supervisor_dispatch;`. |
| `helpers_tests` | `#[cfg(test)] mod` | Test-only. |
| `llm_extraction_tests` | `#[cfg(test)] mod` | Test-only. |
| `reply_loop_tests` | `#[cfg(test)] mod` (commented out) | **Disabled** — see §5.4. |
| `test_helpers` | `#[cfg(test)] pub(crate) mod` | Test-only; used by `djinn-coordinator::test_helpers`. |

---

## 4. Caller/import path inventory

### 4.1 `djinn_agent::actors::slot::*` — external workspace callers

Search command:

```bash
grep -rn "use djinn_agent::actors::slot::" server/crates/ server/src/ | grep -v "\.djinn/"
```

Results (non-test production code):

| File | Import path | Items used |
|------|-------------|------------|
| `server/crates/djinn-agent-worker/src/worker_services.rs:41` | `djinn_agent::actors::slot::helpers::{...}` | Helper functions (format, parse, etc.). |
| `server/src/server/state/mod.rs:12` | `djinn_agent::actors::slot::{SlotPoolConfig, SlotPoolHandle}` | Server state initialization. |
| `server/src/server/state/settings.rs:3` | `djinn_agent::actors::slot::{ModelSlotConfig, SlotPoolConfig}` | Settings deserialization. |
| `server/src/server/chat/prompt/system_message.rs:1` | `djinn_agent::actors::slot::{format_family_for_provider, parse_model_id}` | Prompt assembly. |
| `server/src/server/chat/handler.rs:34` | `djinn_agent::actors::slot::{...}` | Chat handler (likely pool/handle types). |
| `server/src/server/tests/debug.rs:8` | `djinn_agent::actors::slot::{SlotPoolConfig, SlotPoolHandle}` | Test support. |
| `server/src/mcp_bridge/bridges.rs:3` | `djinn_agent::actors::slot::SlotPoolHandle` | MCP bridge pool handle. |

**Compatibility implications:**  
These call sites import through the `djinn_agent::actors::slot::*` facade. When the agent slot tree is eventually removed, each call site must either:
1. Switch to `djinn_slot::*` directly (preferred for new code), or
2. Be preserved by a compatibility shim/re-export in `djinn_agent::actors::slot` that forwards to `djinn_slot`.

The `server/` crate is the primary external consumer; `djinn-agent-worker` is a secondary consumer of helper functions.

### 4.2 `djinn_agent::actors::slot::*` — internal agent-only call sites

Search command:

```bash
grep -rn "use crate::actors::slot::" server/crates/djinn-agent/src/ | grep -v "actors/slot/"
```

Results (non-test production code):

| File | Import path | Items used |
|------|-------------|------------|
| `server/crates/djinn-agent/src/roles/worker.rs:7` | `crate::actors::slot::helpers::initial_user_message_for_task` | Worker role setup. |
| `server/crates/djinn-agent/src/supervisor_impl/stage.rs:74-95` | `crate::actors::slot::{helpers, lifecycle, reply_loop, finalize_handlers}` | Heavy supervisor stage implementation; imports helpers, lifecycle stages, reply-loop types, and teardown. |
| `server/crates/djinn-agent/src/supervisor_impl/pr.rs:32` | `crate::actors::slot::helpers::default_target_branch` | PR supervisor logic. |
| `server/crates/djinn-agent/src/direct_services.rs:92,268,285,288,292,298` | `crate::actors::slot::{helpers, lifecycle::model_resolution}` | Direct LLM invocation services. |
| `server/crates/djinn-agent/src/extension/handlers.rs:287` | `crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger_by_type` | Extension handler skill classification. |
| `server/crates/djinn-agent/src/actors/coordinator/mod.rs:47,117` | `crate::actors::slot::SlotPoolHandle` | Coordinator pool integration; also converts to `djinn_slot::SlotPoolHandle`. |

**Compatibility implications:**  
These are internal to `djinn-agent`. When the local slot tree is deleted, these call sites must either:
- Switch to `djinn_slot::*` directly (if the item is public there), or
- Remain on a thin compatibility shim/re-export inside `djinn-agent`.

The `supervisor_impl/stage.rs` import list is the largest consumer and will need the most careful migration.

### 4.3 `actors::slot::` — broader pattern matches

Search command:

```bash
grep -rn "actors::slot::" server/crates/ server/src/ | grep -v "\.djinn/" | head -40
```

This overlaps heavily with §4.1 and §4.2. Additional matches include:
- `server/crates/djinn-agent/src/prompts/tests/visual_spec.rs` — extensive `crate::actors::slot::lifecycle::{mcp_resolve, task_classifier}` usage in tests.
- `server/crates/djinn-control-plane/tests/execution_tools.rs` — `djinn_agent::actors::slot::TestLifecycleRunner` in tests.
- `server/crates/djinn-agent/src/actors/slot/reply_loop/mod.rs` and `reply_loop/tests.rs` — internal `crate::actors::slot::*` references.
- `server/crates/djinn-slot/src/reply_loop/mod.rs` and `reply_loop/tests.rs` — internal `crate::actors::slot::*` references (these are inside `djinn-slot` itself, using its own module path).

### 4.4 `djinn_slot::*` — direct consumers of the canonical crate

Search command:

```bash
grep -rn "use djinn_slot::" server/crates/ server/src/ | grep -v "\.djinn/" | grep -v "djinn-agent/src/actors/slot/" | grep -v "djinn-agent/src/lib.rs"
```

Results (refreshed after removal of the coordinator doctor check):

| File | Import path | Items used |
|------|-------------|------------|
| `server/crates/djinn-agent-worker/src/worker_services.rs:54` | `djinn_slot::helpers::{…}` | Worker host helpers. |
| `server/crates/djinn-agent/src/context.rs:32` | `djinn_slot::reply_loop::CompactionCriticalSection` | Agent context. |
| `server/crates/djinn-coordinator/src/dispatch/task_dispatch.rs:4276,6783` | `djinn_slot::{ModelSlotConfig, SlotPoolConfig}` | Dispatch test blocks. |
| `server/crates/djinn-coordinator/src/dispatch/session_recovery.rs:9,2839` | `djinn_slot::RunningTaskInfo` | Session recovery and its tests. |
| `server/crates/djinn-coordinator/src/wave.rs:215` | `djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle}` | Coordinator wave logic. |
| `server/crates/djinn-coordinator/src/lib.rs:43` | `djinn_slot::{PoolError, SlotPoolHandle}` | Library surface. |
| `server/crates/djinn-coordinator/src/refinement_pool_watchdog_tests.rs:12` | `djinn_slot::{PoolMessage, PoolStatus, RunningTaskInfo, SlotPoolHandle}` | Refinement pool watchdog tests. |
| `server/crates/djinn-coordinator/src/consolidation.rs:377` | `djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle}` | Consolidation logic. |
| `server/crates/djinn-coordinator/src/actor.rs:29,2030` | `djinn_slot::SlotPoolHandle`, `djinn_slot::{ModelSlotConfig, SlotPoolConfig}` | Coordinator actor. |
| `server/crates/djinn-coordinator/src/rules.rs:1026,1066,1568,1630` | `djinn_slot::{ModelSlotConfig, SlotPoolConfig, SlotPoolHandle}` | Rule engine test blocks. |
| `server/crates/djinn-coordinator/src/test_helpers.rs:15,16` | `djinn_slot::host::SlotContext`, `djinn_slot::reply_loop::CompactionCriticalSection` | Test helpers. |
| `server/crates/djinn-coordinator/src/tests/mod.rs:28` | `djinn_slot::{ModelSlotConfig, SlotHandle, SlotPoolConfig, SlotPoolHandle}` | Integration tests. |
| `server/crates/djinn-coordinator/src/types.rs:14` | `djinn_slot::SlotPoolHandle` | Types module. |

**Compatibility implications:**  
`djinn-coordinator` is already a direct consumer of `djinn_slot`. These call sites are **not** blocked by removing the agent slot tree; they are the intended long-term pattern. Preserving them is trivial as long as `djinn_slot` public API remains stable.

### 4.5 `djinn_slot::*` — inside djinn-agent (shim/re-export wiring)

Inside `djinn-agent` itself, the only non-slot-tree references to `djinn_slot` are in:
- `server/crates/djinn-agent/src/lib.rs:75-76` — re-exports `SlotEvent`, `KnowledgeBranchTarget`, `SlotContext`, `SlotHostCallbacks`.
- `server/crates/djinn-agent/src/actors/slot/mod.rs` — re-exports of `SlotEvent`, memory enrichment, and orchestration types.
- `server/crates/djinn-agent/src/actors/slot/session_extraction.rs` — delegates to `djinn_slot::session_extraction`.
- `server/crates/djinn-agent/src/actors/slot/host_callbacks.rs` — implements `djinn_slot::host::SlotHostCallbacks`.
- `server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs` — called via `djinn_slot::run_supervisor_dispatch` from `actor.rs`.

These are the **intentional shim layer** that must be preserved or migrated during cut-over.

---

## 5. Cross-cutting concerns for later cut-over tasks

### 5.1 Public facade stability

The `djinn_agent::actors::slot::*` facade is still used by `server/`, `djinn-agent-worker`, and `djinn-control-plane` tests. Deleting the agent slot tree without replacing these re-exports will break compilation. A minimal compatibility layer in `djinn-agent/src/actors/slot/mod.rs` (or `lib.rs`) that re-exports `djinn_slot::*` is required as an interim step.

### 5.2 `pub(crate)` depth

Many deeply-internal modules (`lifecycle`, `reply_loop`, `llm_extraction`, `finalize_handlers`) are `pub(crate)` in both trees. When the agent copy is removed, any agent-internal caller (`supervisor_impl/stage.rs`, `direct_services.rs`, `extension/handlers.rs`, `prompts/tests/visual_spec.rs`) that still references `crate::actors::slot::lifecycle::*` will need to either:
- Switch to `djinn_slot::lifecycle::*` (if made public), or
- Be refactored so the needed symbols are exposed through a higher-level public API.

### 5.3 Test modules

Both trees contain:
- `helpers_tests.rs`
- `llm_extraction_tests.rs`
- `reply_loop_tests.rs` (enabled in agent, **commented out** in `djinn-slot`)
- `pool/tests.rs`
- `reply_loop/tests.rs`
- `helpers/tests.rs`

These duplicated tests are a major source of the line-count bloat. The test inventory task (`yz2j`) will decide which copy is canonical and which can be deleted. For this baseline, we note that `djinn-slot/src/lib.rs` explicitly disables `reply_loop_tests` with a comment explaining the breakage (`AgentContext`, `ReplyLoopContext`, missing `test_helpers`).

### 5.4 `reply_loop_tests` disabled in djinn-slot

From `djinn-slot/src/lib.rs` lines 56–62:

```rust
// reply_loop_tests.rs: disabled — tests reference `crate::context::AgentContext`,
// the old ReplyLoopContext struct (with many fields removed during extraction),
// and `crate::test_helpers::test_services` which no longer exists.
// These tests exercise the full reply loop implementation which is still owned
// by djinn-agent. Re-enable after the reply loop is fully extracted to djinn-slot.
// #[cfg(test)]
// mod reply_loop_tests;
```

This is a **known blocker** for the verification epic (`0ecv`). The reply loop must be fully extracted and its tests re-enabled before the agent copy can be removed.

### 5.5 `host_callbacks.rs` — agent-only glue

`djinn-agent/src/actors/slot/host_callbacks.rs` implements `djinn_slot::host::SlotHostCallbacks` for `AgentContext`. This file does **not** exist in `djinn-slot` and is **not** a duplicate; it is host-side glue that must remain in `djinn-agent` even after cut-over. Any plan to delete the agent slot tree must preserve this file (or relocate it to a non-slot directory).

### 5.6 `memory_enrichment.rs` — agent shim

`djinn-agent/src/actors/slot/memory_enrichment.rs` is now a 25-line comment-only shim. The real implementation is in `djinn-slot/src/memory_enrichment.rs`. This shim can be removed once all callers switch to `djinn_slot::memory_enrichment::*` or the re-export in `mod.rs` is sufficient.

### 5.7 `session_extraction.rs` — agent adapter

`djinn-agent/src/actors/slot/session_extraction.rs` is a thin adapter (42 lines) that converts `AgentContext` → `SlotContext` and delegates to `djinn_slot`. Like `memory_enrichment.rs`, this is intentional glue that may need to remain or move.

---

## 6. Summary checklist for downstream tasks

- [ ] **Line-count proof**: Final cut-over must show combined lines dropping from 45,312 to ~30,000 or below (15 k+ reduction). This baseline is the reference.
- [ ] **Facade migration**: `server/`, `djinn-agent-worker`, and `djinn-control-plane` test imports through `djinn_agent::actors::slot::*` must be preserved or migrated.
- [ ] **Internal agent migration**: `supervisor_impl/stage.rs`, `direct_services.rs`, `extension/handlers.rs`, `roles/worker.rs`, `prompts/tests/visual_spec.rs` need compile-checked updates.
- [ ] **Host glue preservation**: `host_callbacks.rs`, `session_extraction.rs` adapter, and `memory_enrichment.rs` shim must not be accidentally deleted.
- [ ] **Reply loop extraction**: `reply_loop_tests` must be re-enabled in `djinn-slot` before agent copy removal.
- [ ] **Test deduplication**: `helpers_tests`, `llm_extraction_tests`, `pool/tests`, `reply_loop/tests` need canonical-source decisions.
- [ ] **Coordinator already clean**: `djinn-coordinator` uses `djinn_slot` directly; no migration needed there.

---

*Generated by task `q7y6` as part of epic `aaiz` — Slot cut-over foundation.*
