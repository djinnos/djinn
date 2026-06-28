# Host-Boundary, Helper, Reply-Loop, Memory, and Pool Slot Reconciliation

> Epic: `aaiz` — Slot cut-over foundation: baseline inventory and reconciliation plan  
> Proposal: `flpe` — Finish the djinn-slot extraction cut-over (de-duplicate the slot subsystem)  
> Artifact: `docs/slot-cutover/host-boundary-reconciliation.md`  
> Scope: documentation-only; no behavior, API, or visibility changes.  
> Companion artifacts: `baseline-inventory.md`, `core-file-reconciliation.md`

---

## Summary table

| Area | Agent path | Slot path | Agent lines | Slot lines | Diff stat | Status |
|------|-----------|-----------|-------------|------------|-----------|--------|
| `supervisor_runner.rs` | `…/slot/supervisor_runner.rs` | `djinn-slot/src/supervisor_runner.rs` | 1,735 | 29 | +17 / −1,723 | **Host-specific glue** (slot is thin shim) |
| `memory_enrichment.rs` | `…/slot/memory_enrichment.rs` | `djinn-slot/src/memory_enrichment.rs` | 25 | 2,391 | +2,390 / −24 | **Already slot-owned** (agent is empty shim) |
| `host_callbacks.rs` / `host.rs` | `…/slot/host_callbacks.rs` | `djinn-slot/src/host.rs` | 165 | 304 | +267 / −128 | **Host callback seam** (different scope per side) |
| `pool/actor.rs` | `…/slot/pool/actor.rs` | `djinn-slot/src/pool/actor.rs` | 1,096 | 948 | +368 / −516 | **Drifted duplicate** (context type swap + feature flag) |
| `pool/handle.rs` | `…/slot/pool/handle.rs` | `djinn-slot/src/pool/handle.rs` | 180 | 199 | +23 / −4 | **Drifted** (slot adds `from_raw_sender` + context swap) |
| `pool/mod.rs` | `…/slot/pool/mod.rs` | `djinn-slot/src/pool/mod.rs` | 11 | 11 | 0 | **Identical** |
| `pool/tests.rs` | `…/slot/pool/tests.rs` | `djinn-slot/src/pool/tests.rs` | 2,157 | 2,157 | +3 / −3 | **Near-identical** (cfg-gate + context swap) |
| `pool/types.rs` | `…/slot/pool/types.rs` | `djinn-slot/src/pool/types.rs` | 159 | 159 | +3 / −3 | **Near-identical** (context type swap) |
| `helpers/code_context.rs` | `…/slot/helpers/code_context.rs` | `djinn-slot/src/helpers/code_context.rs` | 278 | 278 | +1 / −1 | **Near-identical** |
| `helpers/feedback.rs` | `…/slot/helpers/feedback.rs` | `djinn-slot/src/helpers/feedback.rs` | 532 | 534 | +8 / −6 | **Near-identical** (minor doc/import diffs) |
| `helpers/mod.rs` | `…/slot/helpers/mod.rs` | `djinn-slot/src/helpers/mod.rs` | 100 | 87 | +16 / −29 | **Drifted** (visibility + import path changes) |
| `helpers/provider_resolution.rs` | `…/slot/helpers/provider_resolution.rs` | `djinn-slot/src/helpers/provider_resolution.rs` | 740 | 422 | +26 / −344 | **Drifted** (slot stripped host-only credential serialization) |
| `helpers/reviewer_diff.rs` | `…/slot/helpers/reviewer_diff.rs` | `djinn-slot/src/helpers/reviewer_diff.rs` | 233 | 233 | +1 / −1 | **Near-identical** |
| `helpers/tests.rs` | `…/slot/helpers/tests.rs` | `djinn-slot/src/helpers/tests.rs` | 894 | 899 | +6 / −1 | **Near-identical** (minor additions) |
| `reply_loop/budget.rs` | `…/slot/reply_loop/budget.rs` | `djinn-slot/src/reply_loop/budget.rs` | 495 | 23 | +15 / −487 | **Slot is stub** (agent has full policy impl) |
| `reply_loop/error_handling.rs` | `…/slot/reply_loop/error_handling.rs` | `djinn-slot/src/reply_loop/error_handling.rs` | 260 | 10 | +7 / −257 | **Slot is stub** |
| `reply_loop/loop_guard.rs` | `…/slot/reply_loop/loop_guard.rs` | `djinn-slot/src/reply_loop/loop_guard.rs` | 641 | 21 | +13 / −633 | **Slot is stub** |
| `reply_loop/mod.rs` | `…/slot/reply_loop/mod.rs` | `djinn-slot/src/reply_loop/mod.rs` | 34 | 39 | +11 / −6 | **Drifted** (tests disabled in slot) |
| `reply_loop/persistence.rs` | `…/slot/reply_loop/persistence.rs` | `djinn-slot/src/reply_loop/persistence.rs` | 58 | 1 | +1 / −58 | **Slot is stub** (doc-comment only) |
| `reply_loop/streaming.rs` | `…/slot/reply_loop/streaming.rs` | `djinn-slot/src/reply_loop/streaming.rs` | 359 | 1 | +1 / −359 | **Slot is stub** |
| `reply_loop/tests.rs` | `…/slot/reply_loop/tests.rs` | `djinn-slot/src/reply_loop/tests.rs` | 2,410 | 2,410 | +5 / −5 | **Near-identical** (but disabled in slot `lib.rs`) |
| `reply_loop/tool_dispatch.rs` | `…/slot/reply_loop/tool_dispatch.rs` | `djinn-slot/src/reply_loop/tool_dispatch.rs` | 751 | 1 | +1 / −751 | **Slot is stub** |
| `reply_loop/turn.rs` | `…/slot/reply_loop/turn.rs` | `djinn-slot/src/reply_loop/turn.rs` | 1,431 | 23 | +17 / −1,425 | **Slot is stub** |
| `reply_loop_tests.rs` | `…/slot/reply_loop_tests.rs` | `djinn-slot/src/reply_loop_tests.rs` | 532 | 532 | 0 | **Identical** (disabled in both) |
| `helpers_tests.rs` | `…/slot/helpers_tests.rs` | `djinn-slot/src/helpers_tests.rs` | 487 | 487 | 0 | **Identical** |

---

## 1. supervisor_runner.rs — Host-specific dispatch glue

### Paths and line counts

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs` | **1,735** |
| Slot | `server/crates/djinn-slot/src/supervisor_runner.rs` | **29** |

### Diff evidence

```
git diff --no-index --stat .../supervisor_runner.rs .../djinn-slot/src/supervisor_runner.rs
 → 1 file changed, 17 insertions(+), 1723 deletions(-)
```

### Architecture

**Agent side** (1,735 lines) contains the full host-side dispatch logic:

- `supervisor_rpc_span()` — tracing span helper
- `surface_credential_revocation()` — marks stored credentials revoked after HTTP 401; depends on `djinn_provider::repos::CredentialRepository` and `AgentContext.db`
- `dispatch_task_runtime()` — the main host-side dispatch entry point (called from `host_callbacks::AgentDispatchCallbacks::run_task_dispatch`). This function:
  - Loads the task from the DB via `TaskRepository`
  - Resolves conflict/review context via `conflict_context_for_dispatch`
  - Resolves base/task branches from project config
  - Picks the `SupervisorFlow` (NewTask, ReviewResponse, ReviewResume)
  - Implements stage-aware resume (skipping worker redo when output is durable)
  - Resolves per-role model preferences via `resolve_role_model_preference`
  - Resolves read-only multi-repo sources from the task's epic
  - Resolves private-dependency credentials for the worker Pod
  - Constructs a `TaskRunSpec`
  - Dispatches to either `KubernetesRuntime` or `TestRuntime` based on `runtime_kind()`
  - Handles teardown, credential revocation on 401, and provider-failure classification

**Slot side** (29 lines) is a thin shim that delegates everything to the host callback:

```rust
pub async fn run_supervisor_dispatch(
    task_id: String, project_path: String, model_id: String,
    ctx: SlotContext, kill: CancellationToken, pause: CancellationToken,
) -> anyhow::Result<()> {
    ctx.callbacks.run_task_dispatch(task_id, project_path, model_id, ctx.clone(), kill, pause).await
}
```

### Status: Host-specific glue (canonical in agent)

The entire `dispatch_task_runtime` logic depends on `djinn-agent`-internal modules:
- `crate::context::AgentContext`
- `crate::runtime_bridge::{RuntimeKind, SupervisorTaskRunner, runtime_kind}`
- `crate::supervisor::{RoleKind, SupervisorFlow, TaskRunSpec, services_for_agent_context}`
- `crate::roles::flow_for_task_dispatch`
- `crate::actors::slot::lifecycle::model_resolution::resolve_role_model_preference`
- `crate::actors::slot::helpers::*` (conflict_context, target_branch, provider credential, model parsing)
- `djinn_k8s::KubernetesRuntime`

This code cannot live in `djinn-slot` without a reverse dependency on `djinn-agent`, which violates the extraction direction. It must remain behind the `SlotHostCallbacks::run_task_dispatch` callback seam.

### Recommended canonical ownership

- **`djinn-slot/src/supervisor_runner.rs`**: canonical shim — delegates to `SlotHostCallbacks::run_task_dispatch`. This is the slot-side entry point that the slot actor calls.
- **`djinn-agent/src/actors/slot/supervisor_runner.rs`**: remains in `djinn-agent` as host-side dispatch glue. After the cut-over, this becomes the implementation behind `AgentDispatchCallbacks::run_task_dispatch`.

### Manual merge decisions

1. **No merge needed** — the two sides have complementary roles (slot shim vs. host implementation). Neither is a drifted duplicate.
2. If the `dispatch_task_runtime` signature changes, the `host_callbacks.rs` call-site (line 154) must stay in sync.
3. The credential-revocation logic (`surface_credential_revocation`) depends on `djinn_provider::repos::CredentialRepository` and `AgentContext` — this is pure host glue and must remain in `djinn-agent`.

### Required downstream tests/compile checks

- `cargo check -p djinn-agent` — confirms the host-side dispatch still compiles
- `cargo check -p djinn-slot` — confirms the slot shim still compiles
- Integration tests exercising the dispatch path (K8s runtime or test runtime)

---

## 2. memory_enrichment.rs — Already slot-owned

### Paths and line counts

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/memory_enrichment.rs` | **25** |
| Slot | `server/crates/djinn-slot/src/memory_enrichment.rs` | **2,391** |

### Diff evidence

```
git diff --no-index --stat .../memory_enrichment.rs .../djinn-slot/src/memory_enrichment.rs
 → 1 file changed, 2390 insertions(+), 24 deletions(-)
```

### Architecture

**Agent side** (25 lines) is an empty compatibility shim. The file contains only comments explaining that the full implementation has been delegated to `djinn-slot`:

```
// ─── hfhw cutover: memory enrichment delegated to djinn-slot ───────────────
// The full memory enrichment implementation (types, LLM prompt, batch loop,
// entity dedup, edge persistence, `run_memory_enrichment_inner`) now lives
// in `djinn_slot::memory_enrichment`.
// This module is a thin compatibility shim.  The public types and entry
// points are re-exported from `djinn-slot` via `super::mod.rs`:
//   pub use djinn_slot::{ EnrichmentClaim, EnrichmentEdge, ... };
```

**Slot side** (2,391 lines) is the full canonical implementation containing:
- Constants (`ENTITY_DEDUP_COSINE_THRESHOLD`, `MAX_EDGES_PER_BATCH`, `BATCH_SIZE`, etc.)
- Report types: `EnrichmentEntity`, `EnrichmentClaim`, `EnrichmentEdge`, `EnrichmentReport`
- LLM prompt construction and JSON schema
- Batch loop over notes/proposals
- Entity deduplication via cosine similarity
- Edge persistence to `note_associations`
- Entry points: `run_memory_enrichment`, `run_memory_enrichment_with_db`, `run_memory_enrichment_from_context`
- Internal helpers: `run_memory_enrichment_inner`, `extract_edges_from_batch`, etc.

The agent `mod.rs` re-exports the public surface from `djinn-slot`:
```rust
pub use djinn_slot::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport,
    run_memory_enrichment, run_memory_enrichment_with_db,
};
```

### Status: Already slot-owned (agent is empty shim)

The cutover is complete. The agent-side file exists only so `mod memory_enrichment;` resolves. All production code lives in `djinn-slot`.

### Recommended canonical ownership

- **`djinn-slot/src/memory_enrichment.rs`**: canonical implementation.
- **`djinn-agent/src/actors/slot/memory_enrichment.rs`**: can be deleted once no internal agent path resolves `super::memory_enrichment::*` directly (rather than through the `mod.rs` re-exports). The re-exports in `djinn-agent/src/actors/slot/mod.rs` already point at `djinn_slot::*`.

### External callers

`djinn-control-plane` references the memory enrichment types via `djinn_agent::actors::slot::memory_enrichment::*` (in `bridge/memory_enrichment_bridge.rs`). These paths are preserved by the re-exports in `djinn-agent/src/actors/slot/mod.rs`. After the cut-over, callers should switch to `djinn_slot::*` directly or the re-exports should remain as compatibility facades.

### Manual merge decisions

1. **No merge needed** — agent side is already empty.
2. Deletion of the agent-side `memory_enrichment.rs` requires confirming that no code imports from `crate::actors::slot::memory_enrichment::*` within `djinn-agent` itself (grep shows no such usage; the module file exists only for `mod` resolution).

### Required downstream tests/compile checks

- `cargo check -p djinn-agent` — confirm re-exports still resolve
- `cargo check -p djinn-control-plane` — confirm bridge callers still compile
- `cargo test -p djinn-slot memory_enrichment` — run slot-side enrichment tests

---

## 3. Host/Callback Seam — host_callbacks.rs ↔ host.rs

### Paths and line counts

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/host_callbacks.rs` | **165** |
| Slot | `server/crates/djinn-slot/src/host.rs` | **304** |

### Diff evidence

```
git diff --no-index --stat .../host_callbacks.rs .../djinn-slot/src/host.rs
 → 1 file changed, 267 insertions(+), 128 deletions(-)
```

These are **not corresponding files** — they have different scope. The diff is a rename-detection artifact, not a drift comparison.

### Architecture

**Slot side — `host.rs`** (304 lines) defines the host integration seam:

- `KnowledgeBranchTarget` enum — identifies the knowledge-write target for a session
- `ActivityTracker` type alias — per-task last-activity timestamps
- **`SlotHostCallbacks` trait** — the core abstraction. Methods:
  - `interrupt_paused_worker_session` — interrupt a paused session
  - `resolve_mcp_tools` — build MCP tool registry for a worktree+role
  - `render_prompt` — render the prompt for a role
  - `initial_user_message` — build the initial user message
  - `build_mcp_state` — build a control-plane `McpState`
  - `require_project_id_for_task_ops` — resolve project path to project ID
  - `resolve_provider_credential` — resolve provider credential (including OAuth refresh)
  - `run_task_dispatch` — run the full supervisor dispatch (the entry point `supervisor_runner.rs` delegates to)
- `ResolvedMcpTools` struct + `ToolRegistryHandle` trait — opaque MCP tool registry handle
- **`SlotContext` struct** — the slot crate's concrete host context, carrying:
  - `db`, `event_bus`, `catalog`, `health_tracker`, `background_work_tasks`, `active_tasks`
  - `default_project_id`, `working_root`, `coordinator_trigger`, `runtime_ops`, `repo_graph_ops`
  - `callbacks: Arc<dyn SlotHostCallbacks>` — the host callback object
- Helper methods on `SlotContext`: `working_root_for`, `default_project_id`, `knowledge_branch_target_for`, `register_activity`, `touch_activity`, `deregister_activity`, `idle_seconds`, `register_background_work`, `deregister_background_work`, `trigger_dispatch_for_project`, `try_trigger_dispatch`, `mcp_state`, `load_task`

**Agent side — `host_callbacks.rs`** (165 lines) provides the concrete host callback implementation:

- `agent_to_dispatch_slot_context(agent: &AgentContext) -> SlotContext` — constructs a `SlotContext` from `AgentContext` by mapping each field (db, event_bus, catalog, etc.) and creating an `AgentDispatchCallbacks` wrapper
- `AgentDispatchCallbacks` struct — wraps `AgentContext`
- `impl SlotHostCallbacks for AgentDispatchCallbacks`:
  - Most methods are **stubs** (return errors or empty values) because the host-side dispatch path uses `AgentContext` directly
  - `run_task_dispatch` — the only non-stub method; delegates to `super::supervisor_runner::dispatch_task_runtime`

### Status: Host callback seam — correctly split

The slot side owns the trait definition and context types. The agent side owns the concrete implementation. This is the intended architecture.

### Recommended canonical ownership

- **`djinn-slot/src/host.rs`**: canonical. Defines `SlotHostCallbacks`, `SlotContext`, and supporting types.
- **`djinn-agent/src/actors/slot/host_callbacks.rs`**: canonical host wiring. The `AgentDispatchCallbacks` implementation must remain in `djinn-agent` because it wraps `AgentContext` and delegates to `dispatch_task_runtime`.

### Host-only services that must stay behind `SlotHostCallbacks`

The following services are invoked through the callback trait and must **not** introduce a reverse dependency from `djinn-slot` to `djinn-agent` internals:

1. **MCP tool resolution** (`resolve_mcp_tools`) — depends on `djinn-agent::mcp_client`
2. **Prompt rendering** (`render_prompt`) — depends on `djinn-agent::prompts`
3. **Initial user message** (`initial_user_message`) — depends on `djinn-agent::actors::slot::helpers::feedback`
4. **MCP state building** (`build_mcp_state`) — depends on `djinn-control-plane` types wired through agent
5. **Project ID resolution** (`require_project_id_for_task_ops`) — depends on control-plane routing
6. **Provider credential resolution** (`resolve_provider_credential`) — depends on OAuth refresh, vault, credential repository
7. **Full task dispatch** (`run_task_dispatch`) — depends on `runtime_bridge`, `supervisor`, `lifecycle`, `reply_loop`, `roles`

### External callers of `SlotContext` / `SlotHostCallbacks`

- `djinn-agent/src/lib.rs:76` — re-exports `SlotContext`, `SlotHostCallbacks`, `KnowledgeBranchTarget` from `djinn_slot::host`
- `djinn-coordinator/src/test_helpers.rs` — constructs `SlotContext` with a `NoopCallbacks` impl for tests
- `djinn-control-plane` — uses `SlotContext` indirectly through the bridge

### Manual merge decisions

1. **No merge needed** — these files have complementary roles (trait definition vs. implementation).
2. If `SlotHostCallbacks` gains new methods, `AgentDispatchCallbacks` must add corresponding stubs or real implementations.
3. The `agent_to_dispatch_slot_context` function maps `AgentContext` → `SlotContext`; any new field on `SlotContext` must be wired here.

### Required downstream tests/compile checks

- `cargo check -p djinn-slot` — confirms trait and context compile
- `cargo check -p djinn-agent` — confirms implementation compiles
- `cargo check -p djinn-coordinator` — confirms test helpers still compile
- `cargo test -p djinn-slot` — slot-side tests

---

## 4. Pool Modules — pool/actor.rs and related

### 4.1 pool/actor.rs

#### Paths and line counts

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/pool/actor.rs` | **1,096** |
| Slot | `server/crates/djinn-slot/src/pool/actor.rs` | **948** |

#### Diff evidence

```
git diff --no-index --stat .../pool/actor.rs .../djinn-slot/src/pool/actor.rs
 → 1 file changed, 368 insertions(+), 516 deletions(-)
```

#### Drift analysis

The core `SlotPool` struct and its methods are structurally identical. The systematic differences are:

1. **Context type**: agent uses `app_state: AgentContext`, slot uses `ctx: SlotContext`
2. **Feature-gating**: agent uses `#[cfg(test)]` for `test_token_overrides`, slot uses `#[cfg(any(test, feature = "test-support"))]`
3. **Slot factory**: agent inlines `|id, model_id, event_tx, app_state, cancel| { SlotHandle::spawn(...) }`, slot uses `Arc::new(SlotHandle::spawn)` directly
4. **Imports**: agent imports `CoordinatorHandle`, `AgentContext`, `TaskRepository`, `CoordinatorTrigger`; slot imports `SlotContext` and omits coordinator/trigger dependencies
5. **Doc comments**: slot adds a module-level `//!` doc comment

The agent side has 148 more lines because it retains `Duration` and `Arc` imports, inline closure for the slot factory, and more verbose comments. The business logic (pool management, slot allocation, event handling, drain/retire) is structurally identical.

#### Recommended canonical ownership

- **`djinn-slot/src/pool/actor.rs`**: canonical. Uses `SlotContext` and is the slot crate's pool implementation.
- **`djinn-agent/src/actors/slot/pool/actor.rs`**: should be deleted once the agent uses `djinn-slot`'s pool directly. The agent currently constructs `SlotPool` with `AgentContext`; after the cut-over, it should construct via `SlotContext` (using `agent_to_dispatch_slot_context`).

#### Manual merge decisions

1. **Context field rename**: `app_state: AgentContext` → `ctx: SlotContext`. This is the primary drift driver.
2. **Feature gate**: `#[cfg(test)]` → `#[cfg(any(test, feature = "test-support"))]`. The slot side's gate is more permissive and should be canonical.
3. **Slot factory closure**: the agent's inline closure should collapse to `Arc::new(SlotHandle::spawn)` as in slot.
4. **Import cleanup**: agent-side imports of `CoordinatorHandle`, `TaskRepository`, `CoordinatorTrigger` should be removed once the pool runs through `djinn-slot`.

#### Required downstream tests/compile checks

- `cargo test -p djinn-slot pool` — slot pool tests
- `cargo test -p djinn-agent pool` — agent pool tests (post-migration, should use slot pool)

### 4.2 pool/handle.rs

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/pool/handle.rs` | **180** |
| Slot | `server/crates/djinn-slot/src/pool/handle.rs` | **199** |

```
git diff --no-index --stat → +23 / −4
```

Drift: slot adds `from_raw_sender()` constructor (interop seam for `djinn-agent`'s coordinator facade) and swaps `AgentContext` → `SlotContext`. The `from_raw_sender` is a necessary interop primitive and should be canonical in slot.

### 4.3 pool/mod.rs

Identical (11 lines each, 0 diff). No merge decisions.

### 4.4 pool/tests.rs

Near-identical (2,157 lines each, +3 / −3 diff). Differences are cfg-gate (`#[cfg(test)]` vs. `#[cfg(any(test, feature = "test-support"))]`) and context type swap.

### 4.5 pool/types.rs

Near-identical (159 lines each, +3 / −3 diff). Differences are context type swap (`AgentContext` → `SlotContext`).

---

## 5. Helper Modules — helpers/

### 5.1 helpers/mod.rs

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs` | **100** |
| Slot | `server/crates/djinn-slot/src/helpers/mod.rs` | **87** |

```
git diff --no-index --stat → +16 / −29
```

Differences:
- Agent imports `CredentialRepository` and uses `use super::*` (which pulls in the parent module's re-exports); slot imports `SlotContext` directly
- Agent has inline comments for `AUTO_CODE_CONTEXT_ROLES_ENV` and other constants; slot strips some comments
- Agent declares `mod provider_resolution` (private); slot declares `pub mod provider_resolution` (public — needed because slot crate exposes `ProviderCredential` in its public API)
- Both re-export the same public surface from `code_context`, `feedback`, `provider_resolution`, and `reviewer_diff`

**Recommended canonical**: slot version. The visibility change (`pub mod provider_resolution`) is required because `ProviderCredential` is part of the slot crate's public API (used in `SlotHostCallbacks::resolve_provider_credential`).

### 5.2 helpers/code_context.rs

Near-identical (278 lines each, +1 / −1 diff). Trivial comment or whitespace difference.

**Recommended canonical**: either side; no merge decision.

### 5.3 helpers/feedback.rs

Near-identical (532 vs. 534 lines, +8 / −6 diff). Minor doc/import differences.

**Recommended canonical**: slot side (has slightly updated comments).

### 5.4 helpers/provider_resolution.rs

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs` | **740** |
| Slot | `server/crates/djinn-slot/src/helpers/provider_resolution.rs` | **422** |

```
git diff --no-index --stat → +26 / −344
```

**Major drift.** The slot side is 318 lines shorter because:

1. Agent has extensive inline comments on provider identification (MiniMax Anthropic endpoint, Kimi Code subscription, Xiaomi OpenAI-compatible, etc.); slot strips these
2. Agent has `ProviderCredential::to_serializable_credential()` and the full `OAuthConfigWire` / `OAuthCapabilitiesWire` / `OAuthAuthMethodWire` / `OAuthFormatFamilyWire` serde mirror types (used for K8s Secret serialization in Phase 7a); **slot removes all of these** because credential serialization is host-specific
3. Agent has `load_provider_credential()` which calls `CredentialRepository` directly; slot's version delegates to `SlotHostCallbacks::resolve_provider_credential`

**Recommended canonical**: slot side for pure identification functions; agent side retains host-specific credential serialization. The `to_serializable_credential` and wire types are host glue that must stay in `djinn-agent`.

**Manual merge decisions**:
1. Keep the pure identification functions in `djinn-slot` (`format_family_for_provider`, `capabilities_for_provider`, `auth_method_for_provider`, `default_base_url`, `parse_model_id`)
2. Keep `ProviderCredential` enum in `djinn-slot` (it's part of the `SlotHostCallbacks` contract)
3. Keep `to_serializable_credential()` and wire types in `djinn-agent` only (host-specific K8s Secret path)
4. Keep `load_provider_credential()` in `djinn-agent` (calls `CredentialRepository` directly) — the slot version should delegate to callbacks

### 5.5 helpers/reviewer_diff.rs

Near-identical (233 lines each, +1 / −1 diff). No merge decisions.

### 5.6 helpers/tests.rs

Near-identical (894 vs. 899 lines, +6 / −1 diff). Slot side has a few additional test helpers or assertions.

**Recommended canonical**: slot side (superset).

### 5.7 helpers_tests.rs (top-level test module)

Identical (487 lines each, 0 diff). No merge decisions. Both are `#[cfg(test)]` modules registered in their respective `mod.rs` / `lib.rs`.

---

## 6. Reply-Loop Modules — reply_loop/

### 6.1 reply_loop/mod.rs

| Side | Path | Lines |
|------|------|-------|
| Agent | `server/crates/djinn-agent/src/actors/slot/reply_loop/mod.rs` | **34** |
| Slot | `server/crates/djinn-slot/src/reply_loop/mod.rs` | **39** |

```
git diff --no-index --stat → +11 / −6
```

Differences:
- Agent re-exports `ReplyLoopContext` and `run_reply_loop` from `turn` module; slot does **not** (the slot `turn` module is a stub)
- Agent enables `#[cfg(test)] mod tests`; slot has it **commented out** with an explanation:

```rust
// reply_loop_tests.rs: disabled — tests reference types and functions from the
// full reply_loop implementation (BudgetWindDownIgnored, supports_tool_choice_required,
// LoopGuardError, LoopGuardKind, serialize_llm_input, WindDownReason),
// plus djinn-agent-internal modules (crate::actors::slot::*, crate::output_stash,
// crate::supervisor_impl::stage). These modules are stubs in djinn-slot.
// Re-enable after the full reply_loop implementation is extracted to djinn-slot.
// #[cfg(test)]
// #[allow(clippy::await_holding_lock)]
// mod tests;
```

**Recommended canonical**: the slot version documents the disabled-tests fact. The agent version has the live `mod tests` registration. This split is correct for now.

### 6.2 reply_loop submodules — Agent has full implementation, Slot has stubs

| Submodule | Agent lines | Slot lines | Diff | Slot status |
|-----------|-------------|------------|------|-------------|
| `budget.rs` | 495 | 23 | +15 / −487 | **Stub** — simplified `SessionBudget` struct only |
| `error_handling.rs` | 260 | 10 | +7 / −257 | **Stub** — `classify_error` returns `Transient` always |
| `loop_guard.rs` | 641 | 21 | +13 / −633 | **Stub** — `LoopGuard` with basic turn counter |
| `persistence.rs` | 58 | 1 | +1 / −58 | **Stub** — doc comment only |
| `streaming.rs` | 359 | 1 | +1 / −359 | **Stub** — doc comment only |
| `tool_dispatch.rs` | 751 | 1 | +1 / −751 | **Stub** — doc comment only |
| `turn.rs` | 1,431 | 23 | +17 / −1,425 | **Stub** — imports `SlotContext` and stub types |
| `tests.rs` | 2,410 | 2,410 | +5 / −5 | **Near-identical** (but disabled in slot `lib.rs`) |

**Total**: agent reply_loop = 6,435 lines; slot reply_loop = 2,518 lines (of which 2,410 are the disabled test file).

### Architecture

The **agent side** contains the full reply loop implementation:
- `budget.rs` — `SessionBudgetPolicy`, `ResolvedSessionBudget`, role/model-aware budget resolution, env-var overrides, threshold evaluation
- `error_handling.rs` — error classification (`ErrorClass`), provider-failure mapping, retry eligibility
- `loop_guard.rs` — `LoopGuard` with configurable max turns, intervention signals, wind-down reasons
- `persistence.rs` — session-message persistence and serialization helpers
- `streaming.rs` — streaming response handling
- `tool_dispatch.rs` — tool call dispatch via the extension layer
- `turn.rs` — the main `run_reply_loop` orchestrator, `ReplyLoopContext` struct, and all turn-level logic
- `tests.rs` — comprehensive integration test suite

The **slot side** has minimal stubs that compile but do not implement real behavior. The stubs exist so that `djinn-slot` can reference reply-loop types in its module structure (e.g., for future extraction) without depending on the full implementation.

### Status: Full implementation in agent, stubs in slot

The reply loop is the largest remaining un-extracted subsystem. It depends heavily on:
- `djinn-runtime` types (`BiStream`, `StreamEvent`, `LoopGuardKind`, `ProviderFailureClass`, etc.)
- Agent-internal modules (`output_stash`, `supervisor_impl::stage`)
- The `ReplyLoopContext` struct which carries `AgentContext` and runtime-specific fields

### Recommended canonical ownership

- **`djinn-agent/src/actors/slot/reply_loop/`**: canonical for all production reply-loop code until the full extraction to `djinn-slot` happens.
- **`djinn-slot/src/reply_loop/`**: stubs only. The stubs serve as placeholders for the extraction. The `tests.rs` (2,410 lines) is identical to the agent side but disabled — it should be re-enabled only after the full implementation is extracted.

### Manual merge decisions

1. **Do not merge** — the stubs are intentionally minimal. The cut-over must replace the stubs with the real implementation, not merge them.
2. When the reply loop is extracted to `djinn-slot`, the agent-side reply_loop should become a re-export facade (like `memory_enrichment.rs` is today).
3. The `reply_loop_tests.rs` (top-level, 532 lines each, identical in both) is also disabled in `djinn-slot/src/lib.rs`. It references `crate::context::AgentContext` and `crate::test_helpers::test_services` which don't exist in slot.

### Required downstream tests/compile checks

- `cargo test -p djinn-agent reply_loop` — agent-side reply loop tests (live)
- After extraction: `cargo test -p djinn-slot reply_loop` — slot-side tests (currently disabled)
- Workspace compile: `cargo check --workspace` — confirms no import breakage

---

## 7. reply_loop_tests.rs and helpers_tests.rs (top-level)

### Paths and line counts

| File | Agent | Slot | Diff |
|------|-------|------|------|
| `reply_loop_tests.rs` | 532 | 532 | 0 (identical) |
| `helpers_tests.rs` | 487 | 487 | 0 (identical) |

### Status

Both files are identical between agent and slot. Both are `#[cfg(test)]` modules:
- `reply_loop_tests.rs` is registered in agent's `mod.rs` but **commented out** in slot's `lib.rs`
- `helpers_tests.rs` is registered in both sides' module declarations

### Why reply_loop_tests is disabled in djinn-slot

The tests reference types and functions from the full reply-loop implementation that only exists in `djinn-agent`:
- `crate::context::AgentContext`
- `crate::test_helpers::test_services`
- Full `ReplyLoopContext` fields
- `crate::output_stash`, `crate::supervisor_impl::stage`

This is an **inventory fact only** — the tests are not modified in this artifact.

---

## 8. Host-Only Services Summary

The following services must remain behind `SlotHostCallbacks` or equivalent host wiring. They must **not** introduce a reverse dependency from `djinn-slot` to `djinn-agent` internals:

| Service | Callback method | Why host-only |
|---------|----------------|---------------|
| MCP tool resolution | `resolve_mcp_tools` | Depends on `djinn-agent::mcp_client` |
| Prompt rendering | `render_prompt` | Depends on `djinn-agent::prompts` |
| Initial user message | `initial_user_message` | Depends on `helpers::feedback` |
| MCP state building | `build_mcp_state` | Depends on control-plane wiring through agent |
| Project ID resolution | `require_project_id_for_task_ops` | Depends on control-plane routing |
| Provider credential resolution | `resolve_provider_credential` | Depends on OAuth refresh, vault, `CredentialRepository` |
| Full task dispatch | `run_task_dispatch` | Depends on `runtime_bridge`, `supervisor`, `lifecycle`, `reply_loop`, `roles` |
| Credential revocation | N/A (agent-internal) | Depends on `CredentialRepository` + `AgentContext.db` |
| K8s Secret serialization | N/A (agent-internal) | `ProviderCredential::to_serializable_credential` + wire types |

---

## 9. Cross-cutting Dependency Graph

```
djinn-slot::host
  ├── SlotHostCallbacks (trait) ──────────────────── implemented by djinn-agent::host_callbacks
  ├── SlotContext (struct) ────────────────────────── constructed by agent_to_dispatch_slot_context
  └── ToolRegistryHandle (trait) ─────────────────── implemented by djinn-agent (MCP client)

djinn-slot::supervisor_runner
  └── run_supervisor_dispatch ──delegates──► SlotHostCallbacks::run_task_dispatch
                                              └──► djinn-agent::supervisor_runner::dispatch_task_runtime

djinn-slot::memory_enrichment
  └── (full implementation, no host deps except Database/EventBus via SlotContext)

djinn-slot::helpers
  ├── code_context ─── near-identical to agent
  ├── feedback ──────── near-identical to agent
  ├── provider_resolution ── pure identification functions shared; credential loading delegates to host
  └── reviewer_diff ─── near-identical to agent

djinn-slot::reply_loop
  └── (stubs only — full implementation in djinn-agent)

djinn-slot::pool
  ├── actor ──── uses SlotContext (canonical); agent version uses AgentContext (drifted)
  ├── handle ─── uses SlotContext; adds from_raw_sender interop seam
  ├── types ──── near-identical
  └── tests ──── near-identical
```

---

## 10. Reproducible Evidence Commands

All diff and line-count evidence above can be reproduced with:

```bash
# Line counts
wc -l server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs \
      server/crates/djinn-agent/src/actors/slot/memory_enrichment.rs \
      server/crates/djinn-agent/src/actors/slot/pool/actor.rs \
      server/crates/djinn-agent/src/actors/slot/host_callbacks.rs \
      server/crates/djinn-agent/src/actors/slot/helpers/*.rs \
      server/crates/djinn-agent/src/actors/slot/reply_loop/*.rs \
      server/crates/djinn-agent/src/actors/slot/reply_loop_tests.rs \
      server/crates/djinn-agent/src/actors/slot/helpers_tests.rs

wc -l server/crates/djinn-slot/src/supervisor_runner.rs \
      server/crates/djinn-slot/src/memory_enrichment.rs \
      server/crates/djinn-slot/src/pool/actor.rs \
      server/crates/djinn-slot/src/host.rs \
      server/crates/djinn-slot/src/helpers/*.rs \
      server/crates/djinn-slot/src/reply_loop/*.rs \
      server/crates/djinn-slot/src/reply_loop_tests.rs \
      server/crates/djinn-slot/src/helpers_tests.rs

# Diff stats for each area
for f in supervisor_runner memory_enrichment; do
  git diff --no-index --stat "server/crates/djinn-agent/src/actors/slot/$f.rs" "server/crates/djinn-slot/src/$f.rs" || true
done
git diff --no-index --stat server/crates/djinn-agent/src/actors/slot/host_callbacks.rs server/crates/djinn-slot/src/host.rs || true
for f in actor handle mod tests types; do
  git diff --no-index --stat "server/crates/djinn-agent/src/actors/slot/pool/$f.rs" "server/crates/djinn-slot/src/pool/$f.rs" || true
done
for f in code_context feedback mod provider_resolution reviewer_diff tests; do
  git diff --no-index --stat "server/crates/djinn-agent/src/actors/slot/helpers/$f.rs" "server/crates/djinn-slot/src/helpers/$f.rs" || true
done
for f in budget error_handling loop_guard mod persistence streaming tests tool_dispatch turn; do
  git diff --no-index --stat "server/crates/djinn-agent/src/actors/slot/reply_loop/$f.rs" "server/crates/djinn-slot/src/reply_loop/$f.rs" || true
done

# Caller discovery
grep -rn 'supervisor_runner' server/crates/ --include='*.rs'
grep -rn 'memory_enrichment\|run_memory_enrichment' server/crates/ --include='*.rs' | grep -v '/slot/'
grep -rn 'SlotHostCallbacks\|SlotContext\|host_callbacks' server/crates/ --include='*.rs' | grep -v '/djinn-slot/src/' | grep -v '/djinn-agent/src/actors/slot/'
```

---

## 11. Required Downstream Proof Summary

| Check | Command | Scope |
|-------|---------|-------|
| Slot crate compiles | `cargo check -p djinn-slot` | All slot modules including stubs |
| Agent crate compiles | `cargo check -p djinn-agent` | Host callbacks, supervisor_runner, re-exports |
| Control-plane compiles | `cargo check -p djinn-control-plane` | Memory enrichment bridge callers |
| Coordinator compiles | `cargo check -p djinn-coordinator` | Test helpers using SlotContext |
| Agent pool tests | `cargo test -p djinn-agent pool` | Pool actor, handle, types |
| Slot pool tests | `cargo test -p djinn-slot pool` | Pool actor, handle, types |
| Agent reply loop tests | `cargo test -p djinn-agent reply_loop` | Full reply loop (live in agent) |
| Agent helpers tests | `cargo test -p djinn-agent helpers` | Code context, feedback, provider resolution |
| Slot helpers tests | `cargo test -p djinn-slot helpers` | Code context, feedback, provider resolution |
| Workspace compile | `cargo check --workspace` | No import breakage after any migration |
