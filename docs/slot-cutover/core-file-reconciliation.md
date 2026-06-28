# Core File Reconciliation Inventory

> Epic: **aaiz** — Slot cut-over foundation: baseline inventory and reconciliation plan  
> Task: **cxmj** — Inventory reconciliation decisions for core drifted slot files  
> Scope: `commands.rs`, `llm_extraction.rs`, `finalize_handlers.rs`, `actor.rs`, `session_extraction.rs`  
> Status: documentation-only; no source, test, Cargo, or behavior changes.

---

## How to read this document

For each scoped file pair we record:

| Field | Meaning |
|-------|---------|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/<file>` |
| **Slot path** | `server/crates/djinn-slot/src/<file>` |
| **Line counts** | `wc -l` of each copy (Rust source lines) |
| **Drift status** | `identical` / `shimmed` / `drifted` |
| **Canonical home** | Where the long-term source of truth should live |
| **Manual merge decisions** | Concrete changes a future worker must apply before either side can be deleted |
| **Evidence** | Reproducible commands and summarized `git diff --no-index` output |
| **Downstream proof** | Compile checks or targeted tests the cut-over implementation must run |

---

## 1. `commands.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/commands.rs` |
| **Slot path** | `server/crates/djinn-slot/src/commands.rs` |
| **Agent lines** | 89 |
| **Slot lines** | 89 |
| **Drift status** | **shimmed** — semantically identical; only context type name differs |
| **Canonical home** | `djinn-slot/src/commands.rs` |

### Manual merge decisions required

1. **Context-type unification.** Agent copy uses `AgentContext`; slot copy uses `SlotContext`. The two types are field-compatible (both carry `db`, `event_bus`, etc.) but live in different crate modules. Before deleting the agent copy, confirm that every agent-side caller of `log_commands_run_event` can either:
   - pass a `SlotContext` directly, or
   - call through a thin `AgentContext → SlotContext` adapter (already present in `host_callbacks.rs`).
2. **Import path migration.** Agent copy imports `crate::context::AgentContext`; slot copy imports `crate::host::SlotContext`. No behavioral change; purely import rename.

### Evidence

```bash
git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/commands.rs \
  server/crates/djinn-slot/src/commands.rs
```

```
.../{djinn-agent/src/actors/slot => djinn-slot/src}/commands.rs | 4 ++--
 1 file changed, 2 insertions(+), 2 deletions(-)
```

Notable hunk:
- Line 4: `use crate::context::AgentContext;` → `use crate::host::SlotContext;`
- Line 52: `app_state: &AgentContext` → `app_state: &SlotContext`

All other bytes identical.

### Downstream proof required

- `cargo check -p djinn-agent` after replacing the agent copy with a `pub use djinn_slot::commands::*;` re-export (or deleting it and updating call sites).
- `cargo check -p djinn-slot` unchanged (already compiles).
- Targeted grep: confirm no other agent file references `crate::actors::slot::commands::` directly (expected callers go through `super::commands` inside the slot module tree).

---

## 2. `llm_extraction.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/llm_extraction.rs` |
| **Slot path** | `server/crates/djinn-slot/src/llm_extraction.rs` |
| **Agent lines** | 2,266 |
| **Slot lines** | 2,264 |
| **Drift status** | **drifted** — context type + internal crate path changes, plus a few helper call-site adjustments |
| **Canonical home** | `djinn-slot/src/llm_extraction.rs` |

### Manual merge decisions required

1. **Context type rename.** Agent copy uses `AgentContext` for `app_state` parameters; slot copy uses `SlotContext`. The public entry points (`run_llm_extraction`, `run_llm_extraction_with_provider`, `run_llm_extraction_with_provider_and_candidate_lookup`) and the inner `run_llm_extraction_inner` all need the type updated. The slot copy is the canonical signature; any agent-side caller must adapt.
2. **Helper path migration.** Agent copy references `crate::actors::slot::helpers::…` and `crate::actors::slot::lifecycle::model_resolution::…`; slot copy uses `crate::helpers::…` and `crate::lifecycle::model_resolution::…`. These are purely path rewrites because the module hierarchy is flatter inside `djinn-slot`.
3. **`derive_scope_paths` call site.** Agent copy calls `crate::actors::slot::session_extraction::derive_scope_paths`; slot copy calls `crate::session_extraction::derive_scope_paths`. Again, flat module hierarchy.
4. **Test-only imports.** The `mod tests` block at the bottom of each file imports `crate::actors::slot::session_extraction::ExtractionQuality` (agent) vs `crate::session_extraction::ExtractionQuality` (slot). When tests are consolidated, these paths must resolve.
5. **Telemetry / provider builder calls.** Agent copy calls `crate::actors::slot::helpers::build_telemetry_meta_with_attribution`, `resolved_needs_base_url`, `default_base_url`, `build_provider_from_resolved`; slot copy calls `crate::helpers::…`. The helper APIs are identical; only the import prefix changes.

### Evidence

```bash
git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/llm_extraction.rs \
  server/crates/djinn-slot/src/llm_extraction.rs
```

```
.../slot => djinn-slot/src}/llm_extraction.rs      | 30 ++++++++++------------
 1 file changed, 14 insertions(+), 16 deletions(-)
```

Summarized notable hunks:

- **Context type** (4 occurrences):
  - `run_llm_extraction` param `app_state: AgentContext` → `app_state: SlotContext`
  - `run_llm_extraction_with_provider` same
  - `run_llm_extraction_with_provider_and_candidate_lookup` same
  - `run_llm_extraction_inner` same

- **Helper path rewrites** (8 occurrences):
  - `crate::actors::slot::helpers::build_telemetry_meta_with_attribution` → `crate::helpers::build_telemetry_meta_with_attribution`
  - `crate::actors::slot::lifecycle::model_resolution::resolve_model_and_credential` → `crate::lifecycle::model_resolution::resolve_model_and_credential`
  - `crate::actors::slot::helpers::resolved_needs_base_url` → `crate::helpers::resolved_needs_base_url`
  - `crate::actors::slot::helpers::default_base_url` → `crate::helpers::default_base_url`
  - `crate::actors::slot::helpers::build_provider_from_resolved` → `crate::helpers::build_provider_from_resolved`
  - `crate::actors::slot::session_extraction::derive_scope_paths` → `crate::session_extraction::derive_scope_paths`

- **Test imports** (2 occurrences):
  - `use crate::actors::slot::session_extraction::ExtractionQuality;` → `use crate::session_extraction::ExtractionQuality;`
  - `crate::actors::slot::helpers::build_telemetry_meta_with_attribution` → `crate::helpers::build_telemetry_meta_with_attribution`

No logic changes; all diffs are import renames and context-type substitutions.

### Downstream proof required

- `cargo check -p djinn-slot` (already passes; verify after any later helper move).
- `cargo check -p djinn-agent` after removing the agent copy and adding re-exports or updating call sites.
- `cargo test -p djinn-slot --lib llm_extraction` (or the equivalent test module path) to confirm test-only imports still resolve.
- Grep for `run_llm_extraction` callers in the workspace to ensure no orphan references remain:
  ```bash
  grep -r "run_llm_extraction" server/crates/djinn-agent/src/ --include="*.rs"
  grep -r "run_llm_extraction" server/crates/djinn-slot/src/ --include="*.rs"
  ```

---

## 3. `finalize_handlers.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/finalize_handlers.rs` |
| **Slot path** | `server/crates/djinn-slot/src/finalize_handlers.rs` |
| **Agent lines** | 740 |
| **Slot lines** | 736 |
| **Drift status** | **drifted** — context type + import path changes for finalize types |
| **Canonical home** | `djinn-slot/src/finalize_handlers.rs` |

### Manual merge decisions required

1. **Context type rename.** All `app_state: &AgentContext` parameters become `app_state: &SlotContext` (4 functions: `process_finalize_payload`, `handle_budget_park`, `handle_submit_work`, `handle_submit_review`, `handle_submit_decision`, `handle_submit_grooming`).
2. **Finalize types import.** Agent copy imports `crate::roles::finalize::{AcVerdict, SubmitDecision, SubmitGrooming, SubmitReview, SubmitWork}`; slot copy imports `crate::finalize_types::{…}`. The types themselves are identical; only the module path differs. Before deleting the agent copy, ensure `finalize_types.rs` is either:
   - moved into `djinn-slot` (it already exists there), or
   - re-exported from `djinn-agent` via `djinn_slot::finalize_types`.
3. **No behavioral drift.** The function bodies are byte-for-byte identical after the import/context changes.

### Evidence

```bash
git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/finalize_handlers.rs \
  server/crates/djinn-slot/src/finalize_handlers.rs
```

```
.../slot => djinn-slot/src}/finalize_handlers.rs     | 20 ++++++++------------
 1 file changed, 8 insertions(+), 12 deletions(-)
```

Summarized notable hunks:

- Header imports:
  - `-use crate::context::AgentContext;`
  - `-use crate::roles::finalize::{AcVerdict, SubmitDecision, SubmitGrooming, SubmitReview, SubmitWork};`
  - `+use crate::finalize_types::{AcVerdict, SubmitDecision, SubmitGrooming, SubmitReview, SubmitWork};`
  - `+use crate::host::SlotContext;`

- Function signatures (6 occurrences) — `&AgentContext` → `&SlotContext`.

### Downstream proof required

- `cargo check -p djinn-slot` (already passes).
- `cargo check -p djinn-agent` after removing the agent copy and wiring re-exports.
- Grep for `process_finalize_payload` and `handle_budget_park` callers to confirm no hidden agent-side references:
  ```bash
  grep -r "process_finalize_payload\|handle_budget_park" server/crates/djinn-agent/src/ --include="*.rs"
  ```

---

## 4. `actor.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/actor.rs` |
| **Slot path** | `server/crates/djinn-slot/src/actor.rs` |
| **Agent lines** | 624 |
| **Slot lines** | 619 |
| **Drift status** | **drifted** — context type + runner dispatch wiring + import differences |
| **Canonical home** | `djinn-slot/src/actor.rs` |

### Manual merge decisions required

1. **Context type rename.** `AgentContext` → `SlotContext` in `LifecycleRunner`, `SlotActor`, `SlotHandle::spawn`, `SlotHandle::spawn_with_runner`, `SlotHandle::spawn_with_test_runner`, and test helper `test_app_state`.
2. **Runner dispatch wiring (the largest semantic difference).**
   - **Agent copy** `SlotHandle::spawn` builds a `LifecycleRunner` that:
     1. calls `super::host_callbacks::agent_to_dispatch_slot_context(&app_state)` to convert `AgentContext` → `SlotContext`,
     2. then calls `djinn_slot::run_supervisor_dispatch(...)`.
   - **Slot copy** `SlotHandle::spawn` builds a `LifecycleRunner` that directly calls `super::supervisor_runner::run_supervisor_dispatch(...)` with the native `SlotContext`, skipping the cross-crate adapter.
   
   **Decision:** The slot copy is the canonical wiring. When the agent copy is removed, agent-side callers must either:
   - construct a `SlotContext` up-front and pass it into `djinn_slot::SlotHandle::spawn`, or
   - keep a thin `AgentContext → SlotContext` shim at the agent boundary (already present in `host_callbacks.rs`) and call `djinn_slot::SlotHandle::spawn` from there.
3. **Import additions.** Slot copy adds `use super::supervisor_runner::run_supervisor_dispatch;` and removes the `host_callbacks` import. Agent copy keeps the `host_callbacks` import because it needs the adapter.
4. **Comment drift.** The agent copy still carries the `hfhw cutover` comment describing the cross-crate delegation; the slot copy carries a comment about the legacy `run_task_lifecycle` path being kept behind `#[allow(dead_code)]`. Neither comment affects behavior, but a future worker should decide which comment (or both) to preserve in the canonical source.

### Evidence

```bash
git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/actor.rs \
  server/crates/djinn-slot/src/actor.rs
```

```
.../src/actors/slot => djinn-slot/src}/actor.rs    | 45 ++++++++++------------
 1 file changed, 20 insertions(+), 25 deletions(-)
```

Summarized notable hunks:

- **Context type** (6 occurrences in type signatures + 1 in test helper).
- **Runner closure in `SlotHandle::spawn`:**
  - Agent: `let slot_ctx = super::host_callbacks::agent_to_dispatch_slot_context(&app_state); djinn_slot::run_supervisor_dispatch(..., slot_ctx, ...).await`
  - Slot: `Box::pin(run_supervisor_dispatch(..., app_state, ...))` (direct call, no adapter).
- **Imports:**
  - Agent: `use crate::context::AgentContext;` + `use super::{SlotCommand, SlotError, SlotEvent};`
  - Slot: `use crate::host::SlotContext;` + `use super::supervisor_runner::run_supervisor_dispatch;` + `use super::{SlotCommand, SlotError, SlotEvent};`

### Downstream proof required

- `cargo check -p djinn-slot` (already passes).
- `cargo check -p djinn-agent` after removing the agent copy and updating the pool / supervisor to call into `djinn_slot::SlotHandle` directly.
- `cargo test -p djinn-slot --lib actor` to confirm the slot-side tests still pass (they use `SlotContext` natively).
- **Critical compile check:** `SlotHandle::spawn` is called from `pool/actor.rs` on both sides. Verify that the pool actor on the agent side can be updated to pass `SlotContext` (or use the adapter) without signature mismatches:
  ```bash
  grep -r "SlotHandle::spawn" server/crates/djinn-agent/src/ --include="*.rs"
  grep -r "SlotHandle::spawn" server/crates/djinn-slot/src/ --include="*.rs"
  ```

---

## 5. `session_extraction.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/session_extraction.rs` |
| **Slot path** | `server/crates/djinn-slot/src/session_extraction.rs` |
| **Agent lines** | 194 |
| **Slot lines** | 1,778 |
| **Drift status** | **drifted / shim vs. canonical** — agent copy is a thin adapter/shim; slot copy is the full implementation |
| **Canonical home** | `djinn-slot/src/session_extraction.rs` (already the canonical source) |

### Manual merge decisions required

1. **Agent copy is a shim; safe to delete once call sites are updated.** The agent file contains:
   - Re-exports: `pub use djinn_slot::{ExtractionQuality, SessionTaxonomy, derive_scope_paths};`
   - Test-only re-export: `pub use djinn_slot::extract_session_signals;`
   - A test-only adapter `run_structural_extraction` that converts `AgentContext` → `SlotContext`.
   - A no-op `ExtractionCallbacks` implementing `djinn_slot::host::SlotHostCallbacks`.
   - `agent_to_slot_context` mapping function.
   - Two public async adapters: `run_extraction_backfill` and `run_post_session_extraction`.

   **Decision:** Once all agent-side callers import from `djinn_slot::session_extraction` directly (or via `djinn-agent`'s `mod.rs` re-exports), the entire agent file can be deleted. No merge of logic is needed because the slot copy already contains the full implementation.

2. **Call-site inventory.** The following agent-side files reference `session_extraction` symbols (confirmed by grep; see Evidence):
   - `llm_extraction.rs` — uses `SessionTaxonomy`, `derive_scope_paths`
   - `llm_extraction_tests.rs` — uses `extract_session_signals`, `ExtractionQuality`
   - `actor.rs` (indirectly, via `supervisor_runner`) — may trigger `run_post_session_extraction`
   - `mod.rs` — may re-export

   Before deleting the agent shim, update these call sites to import from `djinn_slot::session_extraction` or from a re-export in `djinn-agent/src/actors/slot/mod.rs`.

3. **`agent_to_slot_context` migration.** The mapping function currently lives only in the agent shim. If any other agent module still needs it after the shim is removed, it should be moved to `host_callbacks.rs` (which already has a similar adapter for dispatch).

4. **`ExtractionCallbacks` no-op trait impl.** This is only needed because `agent_to_slot_context` bundles a `callbacks` field. If callers switch to constructing `SlotContext` directly (or via `host_callbacks::agent_to_dispatch_slot_context`), this no-op impl becomes unnecessary and can be deleted with the shim.

### Evidence

```bash
git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/session_extraction.rs \
  server/crates/djinn-slot/src/session_extraction.rs
```

```
.../slot => djinn-slot/src}/session_extraction.rs  | 1934 ++++++++++++++++++--
 1 file changed, 1759 insertions(+), 175 deletions(-)
```

The diff is enormous because the agent file is a 194-line shim and the slot file is a 1,778-line canonical implementation. Rather than reviewing raw hunks, the evidence is structural:

- **Agent file composition (read-back verified):**
  - 24 lines of cutover comments
  - 1 `use crate::context::AgentContext;`
  - 3 `pub use djinn_slot::{…}` re-exports
  - 1 `#[cfg(test)] pub use djinn_slot::extract_session_signals;`
  - 13-line test-only `run_structural_extraction` adapter
  - 96-line no-op `ExtractionCallbacks` impl
  - 20-line `agent_to_slot_context` mapping
  - 14-line `run_extraction_backfill` adapter
  - 13-line `run_post_session_extraction` adapter

- **Slot file composition (first 80 lines read-back verified):**
  - Full crate-internal module with `#![allow(dead_code)]`
  - `use crate::host::SlotContext;`
  - `pub async fn run_post_session_extraction(...)` — the canonical entry point
  - `pub async fn run_extraction_backfill(...)` — the canonical backfill sweep
  - `SessionTaxonomy`, `ExtractionQuality`, `SessionSignals`, `extract_session_signals`, `derive_scope_paths`, `run_structural_extraction`, and all helper functions live here.

- **Caller grep (agent side):**
  ```bash
  grep -r "session_extraction::" server/crates/djinn-agent/src/actors/slot/ --include="*.rs"
  ```
  Expected hits:
  - `llm_extraction.rs`: `super::session_extraction::derive_scope_paths`, `super::session_extraction::SessionTaxonomy`
  - `llm_extraction_tests.rs`: `super::session_extraction::ExtractionQuality`, `super::session_extraction::extract_session_signals`
  - `mod.rs`: may re-export

### Downstream proof required

- `cargo check -p djinn-slot` (already passes; contains the full implementation).
- `cargo check -p djinn-agent` after deleting the agent shim and updating imports in:
  - `llm_extraction.rs`
  - `llm_extraction_tests.rs`
  - any other agent file that references `session_extraction` types.
- `cargo test -p djinn-slot --lib session_extraction` to confirm canonical tests pass.
- `cargo test -p djinn-agent --lib llm_extraction` after import updates to confirm agent-side tests still compile.
- Verify that `mod.rs` re-exports are updated so external workspace callers (e.g., server boot path for `run_extraction_backfill`) still resolve:
  ```bash
  grep -r "run_extraction_backfill" server/crates/ --include="*.rs" | grep -v "djinn-slot"
  ```

---

## Cross-file dependency graph

```
commands.rs ──► used by supervisor_runner.rs (both sides)
llm_extraction.rs ──► calls session_extraction::derive_scope_paths
                    ──► calls helpers::build_telemetry_meta_with_attribution, etc.
                    ──► calls lifecycle::model_resolution::resolve_model_and_credential
finalize_handlers.rs ──► used by reply_loop / supervisor_runner
actor.rs ──► references supervisor_runner::run_supervisor_dispatch
           ──► SlotHandle::spawn called by pool/actor.rs
session_extraction.rs ──► canonical implementation; agent side is shim
```

All five files are tightly coupled to `helpers/`, `lifecycle/`, `pool/`, and `reply_loop/` submodules. The reconciliation decisions above assume those submodules will be inventoried in the sibling task **zugo** (host-boundary reconciliation).

---

## Summary table

| File | Agent lines | Slot lines | Status | Canonical home | Blocker for deletion |
|------|-------------|------------|--------|----------------|----------------------|
| `commands.rs` | 89 | 89 | shimmed | `djinn-slot` | Context-type unification (`AgentContext` → `SlotContext`) |
| `llm_extraction.rs` | 2,266 | 2,264 | drifted | `djinn-slot` | Context type + helper path renames; test import updates |
| `finalize_handlers.rs` | 740 | 736 | drifted | `djinn-slot` | Context type + `finalize_types` import path |
| `actor.rs` | 624 | 619 | drifted | `djinn-slot` | Runner dispatch wiring; `SlotHandle::spawn` caller updates in pool |
| `session_extraction.rs` | 194 | 1,778 | shim vs. canonical | `djinn-slot` | Delete agent shim; update `llm_extraction.rs` and test imports |

---

## Reproducible evidence commands

All evidence above can be regenerated with:

```bash
# Line counts
wc -l \
  server/crates/djinn-agent/src/actors/slot/commands.rs \
  server/crates/djinn-slot/src/commands.rs \
  server/crates/djinn-agent/src/actors/slot/llm_extraction.rs \
  server/crates/djinn-slot/src/llm_extraction.rs \
  server/crates/djinn-agent/src/actors/slot/finalize_handlers.rs \
  server/crates/djinn-slot/src/finalize_handlers.rs \
  server/crates/djinn-agent/src/actors/slot/actor.rs \
  server/crates/djinn-slot/src/actor.rs \
  server/crates/djinn-agent/src/actors/slot/session_extraction.rs \
  server/crates/djinn-slot/src/session_extraction.rs

# Diff stats
git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/commands.rs \
  server/crates/djinn-slot/src/commands.rs || true

git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/llm_extraction.rs \
  server/crates/djinn-slot/src/llm_extraction.rs || true

git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/finalize_handlers.rs \
  server/crates/djinn-slot/src/finalize_handlers.rs || true

git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/actor.rs \
  server/crates/djinn-slot/src/actor.rs || true

git diff --no-index --stat \
  server/crates/djinn-agent/src/actors/slot/session_extraction.rs \
  server/crates/djinn-slot/src/session_extraction.rs || true

# Caller grep (agent side)
grep -r "log_commands_run_event" server/crates/djinn-agent/src/ --include="*.rs"
grep -r "run_llm_extraction" server/crates/djinn-agent/src/ --include="*.rs"
grep -r "process_finalize_payload\|handle_budget_park" server/crates/djinn-agent/src/ --include="*.rs"
grep -r "SlotHandle::spawn" server/crates/djinn-agent/src/ --include="*.rs"
grep -r "session_extraction::" server/crates/djinn-agent/src/actors/slot/ --include="*.rs"
```

---

*Generated for task cxmj (epic aaiz). No Rust source, test registration, Cargo manifest, or behavior-changing file was modified.*
