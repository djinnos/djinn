# Slot Cut-Over Foundation — Index

> Epic: `aaiz` — Slot cut-over foundation: baseline inventory and reconciliation plan  
> Proposal: `flpe` — Finish the djinn-slot extraction cut-over (de-duplicate the slot subsystem)  
> Artifact: `docs/slot-cutover/README.md`  
> Scope: documentation-only; no behavior, API, or visibility changes.

---

## Foundation artifacts

| Artifact | File | What it covers |
|----------|------|----------------|
| **Baseline inventory** | [`baseline-inventory.md`](baseline-inventory.md) | Line counts, file trees, export/module surfaces, caller/import paths for both slot trees. |
| **Core file reconciliation** | [`core-file-reconciliation.md`](core-file-reconciliation.md) | Per-file reconciliation decisions for `commands.rs`, `llm_extraction.rs`, `finalize_handlers.rs`, `actor.rs`, `session_extraction.rs` (canonical source, drift, merge decisions, evidence, compile checks). |
| **Host-boundary reconciliation** | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | Reconciliation for `helpers/`, `reply_loop/`, `pool/`, `memory_enrichment.rs`, `supervisor_runner.rs`, `host_callbacks.rs`, `lifecycle/`, and other boundary modules. |
| **Test inventory** | [`test-inventory.md`](test-inventory.md) | Duplicated/disabled test files (`llm_extraction_tests.rs`, `helpers_tests.rs`, `reply_loop_tests.rs`, module-local tests), commented `reply_loop_tests` registration in `djinn-slot/src/lib.rs`, and consolidation checklist. |

---

## Coverage checklist — every epic-required file/area

The epic scope names the following files/areas. Each is mapped to the artifact that covers it. If an area is not fully covered by an existing artifact, the gap is marked explicitly.

| # | File / area | Covered by | Status |
|---|-------------|------------|--------|
| 1 | `commands.rs` | [`core-file-reconciliation.md`](core-file-reconciliation.md) §1 | ✅ Complete |
| 2 | `llm_extraction.rs` | [`core-file-reconciliation.md`](core-file-reconciliation.md) §2 | ✅ Complete |
| 3 | `finalize_handlers.rs` | [`core-file-reconciliation.md`](core-file-reconciliation.md) §3 | ✅ Complete |
| 4 | `actor.rs` | [`core-file-reconciliation.md`](core-file-reconciliation.md) §4 | ✅ Complete |
| 5 | `session_extraction.rs` | [`core-file-reconciliation.md`](core-file-reconciliation.md) §5 | ✅ Complete |
| 6 | `supervisor_runner.rs` | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | ✅ Complete |
| 7 | `memory_enrichment.rs` | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | ✅ Complete |
| 8 | `helpers/` (mod, code_context, feedback, provider_resolution, reviewer_diff, tests) | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | ✅ Complete |
| 9 | `reply_loop/` (mod, budget, error_handling, loop_guard, persistence, streaming, tool_dispatch, turn, tests) | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | ✅ Complete |
| 10 | `pool/` (mod, actor, handle, types, tests) | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | ✅ Complete |
| 11 | `host_callbacks.rs` | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | ✅ Complete (agent-only glue; not a duplicate) |
| 12 | `lifecycle/` (setup, model_resolution, mcp_resolve, prompt_context, retry, role_overrides, task_classifier, teardown) | [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) | ✅ Complete |
| 13 | `llm_extraction_tests.rs` | [`test-inventory.md`](test-inventory.md) §2.1 | ✅ Complete |
| 14 | `helpers_tests.rs` | [`test-inventory.md`](test-inventory.md) §2.2 | ✅ Complete |
| 15 | `reply_loop_tests.rs` | [`test-inventory.md`](test-inventory.md) §2.3 | ✅ Complete |
| 16 | `helpers/tests.rs` | [`test-inventory.md`](test-inventory.md) §2.4 | ✅ Complete |
| 17 | `reply_loop/tests.rs` | [`test-inventory.md`](test-inventory.md) §2.5 | ✅ Complete |
| 18 | `pool/tests.rs` | [`test-inventory.md`](test-inventory.md) §2.6 | ✅ Complete |
| 19 | Commented `reply_loop_tests` registration in `djinn-slot/src/lib.rs` | [`test-inventory.md`](test-inventory.md) §3 | ✅ Complete |
| 20 | Baseline line counts (`djinn-agent/src/actors/slot` + `djinn-slot/src`) | [`baseline-inventory.md`](baseline-inventory.md) §1 | ✅ Complete |
| 21 | Export/module surface (`mod.rs` + `lib.rs`) | [`baseline-inventory.md`](baseline-inventory.md) §3 | ✅ Complete |
| 22 | Workspace caller inventory (`djinn_agent::actors::slot::*`, `djinn_slot::*`) | [`baseline-inventory.md`](baseline-inventory.md) §4 | ✅ Complete |
| 23 | `host.rs` / `SlotContext` / `SlotHostCallbacks` | [`baseline-inventory.md`](baseline-inventory.md) §3.2 | ✅ Complete |
| 24 | `finalize_types.rs` | [`baseline-inventory.md`](baseline-inventory.md) §3.2 | ✅ Complete |
| 25 | `output_parser.rs` | [`baseline-inventory.md`](baseline-inventory.md) §3.2 | ✅ Complete |
| 26 | `roles_support.rs` | [`baseline-inventory.md`](baseline-inventory.md) §3.2 | ✅ Complete |
| 27 | `truncate.rs` | [`baseline-inventory.md`](baseline-inventory.md) §3.2 | ✅ Complete |
| 28 | `test_helpers.rs` (djinn-slot) | [`test-inventory.md`](test-inventory.md) §4.1 | ✅ Referenced (consolidation noted) |

---

## Gaps and explicit notes

- **`test_helpers` consolidation** is referenced in [`test-inventory.md`](test-inventory.md) §4.1 but does not yet have a dedicated reconciliation decision. The canonical `test_helpers.rs` is in `djinn-slot/src/test_helpers.rs`; the agent copy is in `djinn-agent/src/test_helpers.rs`. A future cut-over task should decide which symbols to merge and which to delete.
- **Lifecycle submodules** (`lifecycle/setup.rs`, `lifecycle/model_resolution.rs`, etc.) are inventoried in [`host-boundary-reconciliation.md`](host-boundary-reconciliation.md) as a group rather than per-file. If a future task needs per-file granularity, it should extend that artifact rather than creating a new one.
- **Reply loop re-enable** is tracked in [`test-inventory.md`](test-inventory.md) §3 with a step-by-step checklist. The actual implementation work belongs to the cut-over implementation epic (`lvft`) and verification epic (`0ecv`).

---

## Downstream epic mapping

| Epic | ID | What it consumes from this foundation |
|------|-----|----------------------------------------|
| Slot cut-over implementation: make djinn-slot the canonical slot crate | `lvft` | Core file reconciliation (core-file-reconciliation.md), host-boundary reconciliation (host-boundary-reconciliation.md), test inventory (test-inventory.md). |
| Slot cut-over host facade: remove djinn-agent duplicate behavior while preserving callers | `p6i4` | Baseline caller inventory (baseline-inventory.md §4), host-boundary reconciliation for `host_callbacks.rs`, `session_extraction.rs` shim, `memory_enrichment.rs` shim. |
| Slot cut-over verification: canonical tests, no disabled modules, and line-count proof | `0ecv` | Test inventory (test-inventory.md), baseline line counts (baseline-inventory.md §1), commented `reply_loop_tests` re-enable checklist. |

---

*Generated by task `yz2j` as part of epic `aaiz` — Slot cut-over foundation.*
