# Slot Cut-Over Implementation Sequence

> Epic: `lvft` — Slot cut-over implementation: make djinn-slot the canonical slot crate  
> Proposal: `flpe` — Finish the djinn-slot extraction cut-over (de-duplicate the slot subsystem)  
> Artifact: `docs/slot-cutover/implementation-sequence.md`  
> Scope: documentation-only; no source, test, Cargo, or behavior changes in this task.

---

## Inputs consumed

This artifact was recovered from the closed spike `b8kb` ("Spike: Convert slot foundation inventories into an implementation cut-over sequence") and the foundation epic `aaiz`. At task start, `docs/slot-cutover/implementation-sequence.md` was **absent** from the current worktree while the following foundation artifacts were present:

- [`docs/slot-cutover/README.md`](README.md) — foundation index and coverage checklist
- [`docs/slot-cutover/baseline-inventory.md`](baseline-inventory.md) — line counts, file trees, export/module surfaces, caller/import paths
- [`docs/slot-cutover/core-file-reconciliation.md`](core-file-reconciliation.md) — per-file reconciliation decisions for `commands.rs`, `llm_extraction.rs`, `finalize_handlers.rs`, `actor.rs`, `session_extraction.rs`
- [`docs/slot-cutover/host-boundary-reconciliation.md`](host-boundary-reconciliation.md) — reconciliation for `helpers/`, `reply_loop/`, `pool/`, `memory_enrichment.rs`, `supervisor_runner.rs`, `host_callbacks.rs`, `lifecycle/`, and other boundary modules
- [`docs/slot-cutover/test-inventory.md`](test-inventory.md) — duplicated/disabled test files, commented `reply_loop_tests` registration, and consolidation checklist

Additional memory notes consumed:

- [`design/lvft-roadmap`](../../reference/lvft-roadmap) — epic roadmap, wave plans, and validation expectations
- [`research/technical/slot-cut-over-implementation-sequence-spike-findings`](../../research/technical/slot-cut-over-implementation-sequence-spike-findings) — spike findings recommending a serialized five-slice wave

---

## Canonical ownership goal

`server/crates/djinn-slot/src/` is the **authoritative implementation surface** for all slot behavior.

`server/crates/djinn-agent/src/actors/slot/` remains **host facade/adapter glue** until the sibling facade epic `p6i4` ("Slot cut-over host facade: remove djinn-agent duplicate behavior while preserving callers") removes duplicate agent files and narrows the facade to pure re-exports and callback wiring.

No reverse dependency from `djinn-slot` to `djinn-agent` is permitted. Host-only services (MCP tool resolution, prompt rendering, credential serialization, K8s runtime dispatch, etc.) remain abstracted behind `SlotHostCallbacks` or explicit slot APIs defined in `djinn-slot/src/host.rs`.

---

## Serialized 5-slice implementation wave

The slices are **serialized** (not parallel) because each touches `djinn-slot/src/lib.rs`, `djinn-agent/src/actors/slot/mod.rs`, and overlapping implementation files. Sibling tasks in the epic map 1:1 to these slices.

| Slice | Task | What it covers | Key files |
|-------|------|----------------|-----------|
| 1 | `pygi` | Core exports and agent facade compatibility | `commands.rs`, `finalize_handlers.rs`, `session_extraction.rs`, `lib.rs`, `mod.rs` |
| 2 | `yfb9` | Helpers, provider boundary, and test-helper support | `helpers/`, `host_callbacks.rs` (agent), `test_helpers.rs` |
| 3 | `w1tr` | Pool ownership behind host adapters | `pool/actor.rs`, `pool/handle.rs`, `pool/types.rs`, `pool/tests.rs` |
| 4 | `xd3x` | Reply-loop extraction and re-enable tests | `reply_loop/`, `reply_loop_tests.rs` |
| 5 | *(part of `xd3x`)* | LLM extraction, actor, memory-enrichment, and supervisor seams | `llm_extraction.rs`, `actor.rs`, `memory_enrichment.rs`, `supervisor_runner.rs` |

---

### Slice 1: Establish canonical core slot exports and agent facade compatibility (`pygi`)

**Goal:** Make `djinn-slot/src/lib.rs` the canonical public surface for low-risk core modules while keeping `djinn-agent/src/actors/slot/mod.rs` as a compatibility re-export facade. Do not delete or rename any agent files.

**Expected file paths**

| Canonical (djinn-slot) | Facade (djinn-agent) | Action |
|------------------------|----------------------|--------|
| `server/crates/djinn-slot/src/commands.rs` | `server/crates/djinn-agent/src/actors/slot/commands.rs` | Replace agent copy with re-export or shim; unify `AgentContext` → `SlotContext` |
| `server/crates/djinn-slot/src/finalize_handlers.rs` | `server/crates/djinn-agent/src/actors/slot/finalize_handlers.rs` | Replace agent copy with re-export; migrate `finalize_types` import path |
| `server/crates/djinn-slot/src/session_extraction.rs` | `server/crates/djinn-agent/src/actors/slot/session_extraction.rs` | Delete agent shim (42-line adapter); update `llm_extraction.rs` and test imports to use `djinn_slot::session_extraction` |
| `server/crates/djinn-slot/src/lib.rs` | `server/crates/djinn-agent/src/actors/slot/mod.rs` | Expand slot exports; add agent re-exports so external callers still compile |

**Overlap / dependency ordering**

- `session_extraction.rs` must be reconciled before `llm_extraction.rs` (Slice 5) because `llm_extraction.rs` calls `derive_scope_paths` and `SessionTaxonomy` from `session_extraction`.
- `finalize_handlers.rs` is used by `reply_loop/` (Slice 4) and `supervisor_runner.rs` (Slice 5); its public surface must be stable before those slices run.
- `commands.rs` is used by `supervisor_runner.rs` on both sides; safe to reconcile early because the drift is only context-type rename.

**Public-surface cautions**

- `djinn-agent::actors::slot::ModelSlotConfig`, `SlotPoolConfig`, `SlotInfo`, `SlotState`, `MERGE_CONFLICT_PREFIX`, `MergeConflictMetadata` are stable shared types from `djinn_orchestration_types`; re-exporting them is safe.
- `djinn-agent::actors::slot::SlotEvent` is defined in `djinn-slot/src/lib.rs`; do not change its definition without checking `server/src/server/state/mod.rs` and `djinn-agent/src/lib.rs` re-exports.
- `run_extraction_backfill` and `run_post_session_extraction` are public entry points; external callers in `server/` may depend on them through the agent facade.

**Validation commands**

```bash
# From server/
cargo check -p djinn-slot --all-features
cargo check -p djinn-agent --all-features
cargo test -p djinn-slot --lib commands
cargo test -p djinn-slot --lib finalize_handlers
cargo test -p djinn-slot --lib session_extraction
```

---

### Slice 2: Reconcile slot helpers, provider-boundary functions, and test-helper support (`yfb9`)

**Goal:** Make `djinn-slot/src/helpers/` the canonical helper surface while keeping host-only credential serialization and direct repository access in `djinn-agent`.

**Expected file paths**

| Canonical (djinn-slot) | Facade / host-only (djinn-agent) | Action |
|------------------------|-----------------------------------|--------|
| `server/crates/djinn-slot/src/helpers/mod.rs` | `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs` | Adopt slot visibility (`pub mod provider_resolution`) in canonical source; agent becomes re-export or thin shim |
| `server/crates/djinn-slot/src/helpers/code_context.rs` | `server/crates/djinn-agent/src/actors/slot/helpers/code_context.rs` | Near-identical; canonicalize in slot |
| `server/crates/djinn-slot/src/helpers/feedback.rs` | `server/crates/djinn-agent/src/actors/slot/helpers/feedback.rs` | Near-identical; canonicalize in slot |
| `server/crates/djinn-slot/src/helpers/provider_resolution.rs` | `server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs` | Slot keeps pure identification functions; agent keeps `to_serializable_credential()`, wire types, and `load_provider_credential()` (host-only) |
| `server/crates/djinn-slot/src/helpers/reviewer_diff.rs` | `server/crates/djinn-agent/src/actors/slot/helpers/reviewer_diff.rs` | Near-identical; canonicalize in slot |
| `server/crates/djinn-slot/src/helpers/tests.rs` | `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` | Canonicalize in slot (superset) |
| `server/crates/djinn-slot/src/helpers_tests.rs` | `server/crates/djinn-agent/src/actors/slot/helpers_tests.rs` | Canonicalize in slot; delete agent copy once call sites compile |
| `server/crates/djinn-slot/src/test_helpers.rs` | `server/crates/djinn-agent/src/test_helpers.rs` | Consolidate required symbols into slot `test_helpers.rs`; agent may re-export or keep its own |

**Overlap / dependency ordering**

- Must run after Slice 1 because `helpers/mod.rs` re-exports may depend on `commands.rs` and `session_extraction.rs` public symbols being stable.
- `provider_resolution.rs` is referenced by `llm_extraction.rs` (Slice 5) and `actor.rs` / `pool/actor.rs` (Slice 3); its public API (`format_family_for_provider`, `parse_model_id`, `default_base_url`, etc.) must be finalized before those slices.
- `test_helpers.rs` consolidation is a blocker for re-enabling `reply_loop_tests` (Slice 4) because the disabled tests reference `test_services()` and context builders.

**Public-surface cautions**

- `helpers::provider_resolution` is `pub mod` in slot but `mod` (private) in agent. Changing agent visibility to match slot is safe because the slot crate already exposes it publicly.
- `ProviderCredential` is part of the `SlotHostCallbacks` contract; do not narrow its visibility.
- `initial_user_message_for_task` is used by `djinn-agent-worker/src/worker_services.rs` and `server/src/server/chat/prompt/system_message.rs` through the agent facade; preserve the re-export path.

**Validation commands**

```bash
# From server/
cargo check -p djinn-slot --all-features
cargo test -p djinn-slot --lib helpers::tests
cargo test -p djinn-slot --lib helpers_tests
cargo check -p djinn-agent --all-features
cargo check -p djinn-agent-worker --all-features
```

---

### Slice 3: Move slot pool ownership to djinn-slot behind host adapters (`w1tr`)

**Goal:** Make `djinn-slot::pool` and `SlotHandle` the canonical implementation surface while the agent boundary constructs `SlotContext` through host adapters.

**Expected file paths**

| Canonical (djinn-slot) | Facade (djinn-agent) | Action |
|------------------------|----------------------|--------|
| `server/crates/djinn-slot/src/pool/actor.rs` | `server/crates/djinn-agent/src/actors/slot/pool/actor.rs` | Canonicalize slot version (`SlotContext`, `#[cfg(any(test, feature = "test-support"))]`); agent becomes re-export or thin shim |
| `server/crates/djinn-slot/src/pool/handle.rs` | `server/crates/djinn-agent/src/actors/slot/pool/handle.rs` | Canonicalize slot version (`from_raw_sender` interop seam); agent becomes re-export |
| `server/crates/djinn-slot/src/pool/mod.rs` | `server/crates/djinn-agent/src/actors/slot/pool/mod.rs` | Identical; canonicalize in slot |
| `server/crates/djinn-slot/src/pool/types.rs` | `server/crates/djinn-agent/src/actors/slot/pool/types.rs` | Canonicalize slot version (`SlotContext` field) |
| `server/crates/djinn-slot/src/pool/tests.rs` | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs` | Canonicalize in slot; delete agent copy once `SlotHandle::spawn` signature is stable |

**Overlap / dependency ordering**

- Must run after Slice 2 because `pool/actor.rs` uses `helpers::provider_resolution` and `helpers::feedback` (via `SlotHandle::spawn` → `LifecycleRunner`).
- `SlotHandle::spawn` is called from `pool/actor.rs` on both sides; the signature change (`AgentContext` → `SlotContext`) must be coordinated with the agent's `host_callbacks::agent_to_dispatch_slot_context` adapter.
- `pool/handle.rs` `from_raw_sender()` is used by `djinn-agent/src/actors/coordinator/mod.rs` as an interop seam; verify that caller still compiles after canonicalization.

**Public-surface cautions**

- `SlotPoolHandle`, `SlotPoolConfig`, `PoolError` are public re-exports used by `server/src/server/state/mod.rs`, `server/src/mcp_bridge/bridges.rs`, and `djinn-coordinator`. Do not rename or remove.
- `SlotHandle::spawn` and `SlotHandle::spawn_with_runner` are public; signature changes require updating `pool/actor.rs` and agent-side callers.

**Validation commands**

```bash
# From server/
cargo check -p djinn-slot --all-features
cargo test -p djinn-slot --lib pool::tests
cargo check -p djinn-agent --all-features
cargo check -p djinn-coordinator --all-features
```

---

### Slice 4: Extract reply-loop coverage and reconcile reply-loop tests (`xd3x` — part A)

**Goal:** Replace the stubs in `djinn-slot/src/reply_loop/` with the full implementation from `djinn-agent`, re-enable `reply_loop_tests` in `djinn-slot/src/lib.rs`, and make the agent reply-loop a re-export facade.

**Expected file paths**

| Canonical (djinn-slot) | Facade (djinn-agent) | Action |
|------------------------|----------------------|--------|
| `server/crates/djinn-slot/src/reply_loop/mod.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/mod.rs` | Slot: enable `mod tests;`, re-export `ReplyLoopContext` and `run_reply_loop`; Agent: become re-export or thin shim |
| `server/crates/djinn-slot/src/reply_loop/budget.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/budget.rs` | Replace slot stub (23 lines) with full agent implementation (495 lines) |
| `server/crates/djinn-slot/src/reply_loop/error_handling.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/error_handling.rs` | Replace slot stub (10 lines) with full agent implementation (260 lines) |
| `server/crates/djinn-slot/src/reply_loop/loop_guard.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/loop_guard.rs` | Replace slot stub (21 lines) with full agent implementation (641 lines) |
| `server/crates/djinn-slot/src/reply_loop/persistence.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/persistence.rs` | Replace slot stub (1 line) with full agent implementation (58 lines) |
| `server/crates/djinn-slot/src/reply_loop/streaming.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/streaming.rs` | Replace slot stub (1 line) with full agent implementation (359 lines) |
| `server/crates/djinn-slot/src/reply_loop/tool_dispatch.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/tool_dispatch.rs` | Replace slot stub (1 line) with full agent implementation (751 lines) |
| `server/crates/djinn-slot/src/reply_loop/turn.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/turn.rs` | Replace slot stub (23 lines) with full agent implementation (1,431 lines) |
| `server/crates/djinn-slot/src/reply_loop/tests.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/tests.rs` | Canonicalize in slot (already near-identical); delete agent copy |
| `server/crates/djinn-slot/src/reply_loop_tests.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop_tests.rs` | Re-enable in slot `lib.rs`; rewrite `make_context` to use `SlotContext`; delete agent copy once passing |

**Overlap / dependency ordering**

- Must run after Slice 3 because `reply_loop/turn.rs` calls `SlotHandle::spawn` and references `pool/` types.
- Must run after Slice 2 because `reply_loop/` uses `helpers::feedback`, `helpers::code_context`, and `test_helpers`.
- Must run after Slice 1 because `reply_loop` uses `finalize_handlers.rs` and `session_extraction.rs`.
- The `reply_loop_tests.rs` re-enable depends on `test_helpers` consolidation (Slice 2) and `AgentContext` → `SlotContext` migration (Slice 1).
- This slice must **not** run in parallel with any other slice because it touches the largest remaining un-extracted subsystem and the most test infrastructure.

**Public-surface cautions**

- `ReplyLoopContext` and `run_reply_loop` are public re-exports in agent `reply_loop/mod.rs`; slot must expose them at the same paths or the agent facade must re-export them.
- `reply_loop_tests.rs` is currently commented out in `djinn-slot/src/lib.rs` (lines 56–62). Do not uncomment until the test's `AgentContext` references are rewritten and `test_services()` is available in slot `test_helpers.rs`.
- The reply loop depends on `djinn-runtime` types (`BiStream`, `StreamEvent`, `LoopGuardKind`, `ProviderFailureClass`). Ensure `djinn-slot` Cargo.toml has the required dependencies before extraction.

**Validation commands**

```bash
# From server/
cargo check -p djinn-slot --all-features
cargo test -p djinn-slot --lib reply_loop::tests
# After re-enable:
# cargo test -p djinn-slot --lib reply_loop_tests
cargo check -p djinn-agent --all-features
```

---

### Slice 5: Reconcile LLM extraction, actor, memory-enrichment, and supervisor seams (`xd3x` — part B)

**Goal:** Move the remaining canonical implementations into `djinn-slot` while keeping host dispatch glue in `djinn-agent`. This slice is the final code-reconciliation step before the facade/deletion epic `p6i4` takes over.

**Expected file paths**

| Canonical (djinn-slot) | Facade / host-only (djinn-agent) | Action |
|------------------------|-----------------------------------|--------|
| `server/crates/djinn-slot/src/llm_extraction.rs` | `server/crates/djinn-agent/src/actors/slot/llm_extraction.rs` | Canonicalize slot version; agent becomes re-export or thin shim; update test imports |
| `server/crates/djinn-slot/src/actor.rs` | `server/crates/djinn-agent/src/actors/slot/actor.rs` | Canonicalize slot version (`SlotContext`, direct `supervisor_runner::run_supervisor_dispatch`); agent keeps adapter path via `host_callbacks` if needed |
| `server/crates/djinn-slot/src/memory_enrichment.rs` | `server/crates/djinn-agent/src/actors/slot/memory_enrichment.rs` | Already canonical in slot (2,391 lines); agent shim (25 lines) can be deleted once `mod.rs` re-exports are verified |
| `server/crates/djinn-slot/src/supervisor_runner.rs` | `server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs` | Slot remains thin callback shim (29 lines); agent remains host dispatch glue (1,735 lines) |
| `server/crates/djinn-slot/src/llm_extraction_tests.rs` | `server/crates/djinn-agent/src/actors/slot/llm_extraction_tests.rs` | Canonicalize in slot; delete agent copy once `llm_extraction.rs` canonical source is stable |

**Overlap / dependency ordering**

- Must run after Slice 4 because `actor.rs` `SlotHandle::spawn` references `reply_loop` types and `supervisor_runner::run_supervisor_dispatch`.
- Must run after Slice 2 because `llm_extraction.rs` calls `helpers::build_telemetry_meta_with_attribution`, `helpers::provider_resolution::resolve_model_and_credential`, and `session_extraction::derive_scope_paths`.
- `memory_enrichment.rs` is already canonical in slot; the only work is verifying the agent shim can be deleted (deferred to `p6i4`).
- `supervisor_runner.rs` has complementary roles on each side (slot shim vs. host implementation); no merge is needed. The slot shim delegates to `SlotHostCallbacks::run_task_dispatch`, which is implemented by `djinn-agent/src/actors/slot/host_callbacks.rs`.

**Public-surface cautions**

- `llm_extraction.rs` public entry points (`run_llm_extraction`, `run_llm_extraction_with_provider`, `run_llm_extraction_with_provider_and_candidate_lookup`) are used by `djinn-agent/src/direct_services.rs`. Preserve the re-export path or update the caller.
- `actor.rs` exports `SlotActor`, `SlotHandle`, `TestLifecycleRunner`. These are used by `djinn-coordinator`, `djinn-control-plane` tests, and `server/`. Do not rename or remove.
- `run_supervisor_dispatch` is re-exported from `djinn-slot/src/lib.rs` and called by `djinn-agent/src/actors/slot/actor.rs` (agent copy) through the host callback path. The slot shim signature must stay in sync with `SlotHostCallbacks::run_task_dispatch`.

**Validation commands**

```bash
# From server/
cargo check -p djinn-slot --all-features
cargo test -p djinn-slot --lib llm_extraction
cargo test -p djinn-slot --lib actor
cargo test -p djinn-slot --lib memory_enrichment
cargo check -p djinn-agent --all-features
cargo check -p djinn-control-plane --all-features
cargo check -p djinn-coordinator --all-features
```

---

## Host-boundary cautions

### `supervisor_runner.rs` — callback seam, not a drifted duplicate

- `djinn-slot/src/supervisor_runner.rs` (29 lines) is a **thin callback shim** that delegates to `SlotHostCallbacks::run_task_dispatch`. It must remain in `djinn-slot` as the slot-side entry point.
- `djinn-agent/src/actors/slot/supervisor_runner.rs` (1,735 lines) is **host dispatch glue** containing `dispatch_task_runtime`, credential revocation, K8s runtime dispatch, and supervisor flow resolution. It must remain in `djinn-agent` because it depends on `AgentContext`, `runtime_bridge`, `supervisor`, `roles`, and `KubernetesRuntime`.
- **No reverse dependency:** `djinn-slot` must never import from `djinn-agent` internals. The host callback trait (`SlotHostCallbacks`) is the only permitted seam.

### `memory_enrichment.rs` — already canonical

- The canonical implementation (2,391 lines) is already in `djinn-slot`. The agent file is a 25-line comment-only shim. Deletion of the agent shim is deferred to epic `p6i4`.

### `host_callbacks.rs` — agent-only implementation

- `djinn-agent/src/actors/slot/host_callbacks.rs` implements `SlotHostCallbacks` for `AgentContext`. It must remain in `djinn-agent` because it wraps `AgentContext` and delegates to `supervisor_runner::dispatch_task_runtime`.
- If `SlotHostCallbacks` gains new methods, `AgentDispatchCallbacks` must add corresponding implementations.

---

## Destructive-change caution

This wave **must not delete or rename agent files**. The following actions are forbidden in slices 1–5:

- Deleting `djinn-agent/src/actors/slot/*.rs` or `djinn-agent/src/actors/slot/**/*.rs`
- Renaming public structs, enums, traits, or functions exported from `djinn-agent::actors::slot::*`
- Narrowing visibility of `pub` or `pub use` items in `djinn-agent/src/actors/slot/mod.rs`
- Removing `mod` declarations from `djinn-agent/src/actors/slot/mod.rs` before the corresponding `djinn-slot` re-export is proven to compile for all callers
- Changing `Cargo.toml` dependencies in either crate
- Re-enabling or disabling `#[cfg(test)] mod` registrations in a way that breaks existing test suites

Future deletion, signature-narrowing, or agent-slot-tree removal requires a **destructive-change impact preflight** if that tool is available in the session. If not available, the planner must manually verify all callers listed in `baseline-inventory.md` §4 before creating deletion tasks.

---

## Validation expectations for the full wave

Run from `server/` as far as each slice allows:

| Stage | Command | Expected result |
|-------|---------|-----------------|
| After every slice | `cargo build -p djinn-slot --all-features` | Must pass |
| After every slice | `cargo build -p djinn-agent --all-features` | Must pass (agent compiles against slot re-exports) |
| After Slice 2 | `cargo test -p djinn-slot --lib helpers_tests` | Must pass |
| After Slice 3 | `cargo test -p djinn-slot --lib pool::tests` | Must pass |
| After Slice 4 | `cargo test -p djinn-slot --lib reply_loop::tests` | Must pass |
| After Slice 4 (re-enable) | `cargo test -p djinn-slot --lib reply_loop_tests` | Must pass |
| After Slice 5 | `cargo test -p djinn-slot --lib llm_extraction_tests` | Must pass |
| After Slice 5 | `cargo test -p djinn-slot --lib actor` | Must pass |
| Full wave | `cargo check --workspace` | Document any blockers; do not disable tests to force green |

If a workspace-wide command cannot pass until the subsequent facade/deletion epic `p6i4`, document the **exact blocking compile errors** and keep the branch in a state that the next epic can consume.

---

## Downstream epic mapping

| Epic | ID | What it consumes from this sequence |
|------|-----|----------------------------------------|
| Slot cut-over host facade: remove djinn-agent duplicate behavior while preserving callers | `p6i4` | This sequence's validation commands, public-surface cautions, and destructive-change rules. Deletes agent duplicates after compile-checkable slot APIs exist. |
| Slot cut-over verification: canonical tests, no disabled modules, and line-count proof | `0ecv` | Re-enabled `reply_loop_tests`, consolidated `test_helpers`, deleted agent test duplicates, and line-count reduction from 45,312 to ~30,000. |

---

*Recovered by task `g91p` as part of epic `lvft` — Slot cut-over implementation.*
