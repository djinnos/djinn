# Trait-Dispatch Validation Matrix and Confidence Semantics

> **Epic:** `h1hn` — Validate trait-dispatch graph behavior end-to-end in `djinnos/djinn`
> **Proposal:** `t16t` — Synthesize trait/interface-dispatch call edges in the canonical graph
> **Status:** All wave-1 validation tasks closed and merged (`9nqu`, `wvsn`, `qjo3`, `p9cw`).

This document is the developer-facing reference for the trait-dispatch validation wave.
It records the locally reproducible test matrix, warm-artifact expectations, the finalized
fan-out default/cap and confidence/provenance semantics, and the three-method reproduction
corpus. Future maintainers can use it to audit or regress the behavior without needing
production credentials or operator access.

---

## Table of Contents

- [Reproduction Corpus](#reproduction-corpus)
- [Validation Matrix (Locally Reproducible)](#validation-matrix-locally-reproducible)
- [Warm-Artifact Expectations](#warm-artifact-expectations)
- [Fan-Out Default / Cap](#fan-out-default--cap)
- [Confidence / Provenance Semantics](#confidence--provenance-semantics)
- [Synthesized vs. Directly Extracted Edges](#synthesized-vs-directly-extracted-edges)
- [`min_confidence` Behavior](#min_confidence-behavior)
- [Deployed / Manual UI Verification Checklist](#deployed--manual-ui-verification-checklist)
- [Follow-Up Recommendations](#follow-up-recommendations)

---

## Reproduction Corpus

The hand-verified corpus is checked into:
`server/src/mcp_bridge/graph_ops/tests/trait_dispatch_corpus.rs`

It contains **4 entries** (≥3 required by proposal `t16t`):

| # | Trait method | Declaration file:line | Concrete impl(s) | Production caller(s) |
|---|-------------|-----------------------|-------------------|----------------------|
| 1 | `RuntimeOps::list_taskrun_jobs` | `runtime_bridge.rs:137` | `AppState` (`mod.rs:78`), `StubRuntime` (`test_support.rs:168`), `StubRuntimeOps` (`state.rs:437`) | `reap_orphaned_taskrun_jobs` (`health.rs:1490`) |
| 2 | `RepoGraphOps::context` | `graph_bridge.rs:298` | `RepoGraphBridge` (`graph_ops/mod.rs:240`) | `GraphToolHandler::code_graph_context` (`handler_basic_ops.rs:720`) |
| 3 | `SlotPoolOps::get_status` | `slot_pool_bridge.rs:36` | `SlotPoolBridge` (`bridges.rs:42`), `StubSlotPool` (`test_support.rs:83`) | `CoordinatorActor::reconcile_inflight_dispatch_ledger` (`task_dispatch.rs:243`), `CoordinatorActor::maybe_consolidate_idle_slots` (`actor.rs:1576`) |
| 4 | `RepoGraphOps::impact` | `graph_bridge.rs:101` | `RepoGraphBridge` (`graph_ops/mod.rs:118`) | `GraphToolHandler::code_graph_impact` (`handler_basic_ops.rs:204`) |

Each entry records the trait method declaration symbol/path, concrete impl method symbol/path(s),
and hand-verified caller symbols/files. All file paths are relative to the repository root.

### Verification Methodology

The corpus entries were hand-verified using:
1. **`code_search` / `grep`** — identify the trait declaration, every `impl Trait for Concrete` block,
   and every call site where the trait method is invoked on a trait-object or generic-typed receiver.
2. **Source reading** — confirm the call goes through the trait (not a concrete inherent method) by
   verifying the receiver type is `dyn Trait`, `Arc<dyn Trait>`, or a generic `T: Trait`/`&impl Trait`.
3. **`grep -n`** — record exact file paths and 1-based line numbers so downstream tests can assert
   symbol-file correspondence without re-scanning.

The mandatory entry `RuntimeOps::list_taskrun_jobs` was cross-checked with the 9nqu reviewer, who
confirmed exact file paths and symbol names for all recorded locations.

---

## Validation Matrix (Locally Reproducible)

All tests run **without production graph data, Kubernetes, Docker-only services, or operator
credentials**. Fixtures are in-memory `RepoDependencyGraph` instances built from
`RepoGraphArtifact`.

| Surface | Command | What it validates |
|---------|---------|-------------------|
| Corpus structure | `cd server && cargo test -p djinn-server trait_dispatch_corpus::tests --all-features` | 6 structural tests (count ≥3, mandatory entry present, non-empty paths, positive lines, valid callers, valid test callers) |
| Context / neighbors | `cd server && cargo test -p djinn-server trait_dispatch_query --all-features` | 10 query regression tests for `context`/`neighbors` with synthetic fixtures |
| Impact BFS | `cd server && cargo test -p djinn-server trait_dispatch_impact --all-features` | 10 impact tests proving `min_confidence` gating at the 0.70 floor |
| Corpus e2e | `cd server && cargo test -p djinn-server trait_dispatch_corpus_e2e --all-features` | 12 end-to-end tests tying all 4 corpus entries to `collect_context_buckets` and `impact_bfs` |
| Agent dispatch | `cd server && cargo test -p djinn-agent code_graph_tests --all-features` | 4 agent-level dispatch tests for `RuntimeOps::list_taskrun_jobs` |
| UI component | `cd ui && pnpm test -- SymbolDetailPanel` | 11 tests including the new trait-dispatch rendering test |

### Full workspace no-regression

To verify no regression in the broader server test suite (run by CI, not required per-task):

```bash
cd server && cargo test -p djinn-server -p djinn-graph -p djinn-agent --all-features
```

### UI no-regression

```bash
cd ui && pnpm test
```

---

## Warm-Artifact Expectations

- **Artifact version:** `v11` (`REPO_GRAPH_ARTIFACT_VERSION = 11` in
  `server/crates/djinn-graph/src/repo_graph/constants.rs`).
- **Re-warm trigger:** Old v10 artifacts bincode-fail on the new `TraitDispatchCall` enum variant
  and force a re-warm.
- **Additive behavior:** The new edge kind is purely additive. Existing `FileReference`,
  `SymbolReference`, `Reads`, `Writes`, `Implements`, `Defines`, and other edge kinds are preserved.
  `TraitDispatchCall` edges are stamped only for Rust trait-method occurrences that resolve to an
  in-repo declared symbol.
- **Local warm check (optional):** If you have a project clone and a SCIP index, build the graph with
  the `djinn-graph` builder and verify:
  1. `RepoGraphArtifact.version == 11`
  2. `TraitDispatchCall` edges appear for the corpus symbols (e.g. `RuntimeOps::list_taskrun_jobs`).

  This requires a local project + SCIP indexer setup; it is **not** a worker acceptance criterion.

---

## Fan-Out Default / Cap

- **Constant:** `TRAIT_DISPATCH_FANOUT_CAP = 5`
  (defined in `server/crates/djinn-graph/src/repo_graph/constants.rs`, re-exported from
  `server/crates/djinn-graph/src/repo_graph/mod.rs`).

- **Behavior:** When a trait method has ≤5 known concrete implementations (from SCIP
  `Implementation` relationships), the builder emits direct `caller → impl_method`
  `TraitDispatchCall` edges **in addition to** the `caller → trait_method` edge. When the cap is
  exceeded (>5 impls), impl fan-out is suppressed — only the direct `caller → trait_method` edge is
  emitted.

- **Rationale:** Most traits in real Rust codebases have fewer than 5 implementations. A cap of 5
  covers the common case while preventing pathological edge multiplication for widely-implemented
  framework traits (e.g. `RuntimeOps` with 10+ stub impls in tests).

- **Implementation:** See `maybe_add_trait_dispatch_call` in
  `server/crates/djinn-graph/src/repo_graph/builder.rs` (around line 422).

---

## Confidence / Provenance Semantics

| Concept | Value | Location |
|---------|-------|----------|
| Edge kind | `RepoGraphEdgeKind::TraitDispatchCall` | `edge.rs` |
| Confidence floor | `0.70` | `constants.rs` (`EDGE_CONFIDENCE_TRAIT_DISPATCH_CALL`) |
| Confidence tier | `Inferred` (below 0.9 threshold) | `edge.rs::edge_confidence_tier()` |
| Edge weight | `1.5` | `constants.rs` (`EDGE_WEIGHT_TRAIT_DISPATCH_CALL`) |
| Reason (direct) | `"trait-dispatch-call"` | `constants.rs` (`REASON_TRAIT_DISPATCH_CALL`) |
| Reason (fan-out) | `"trait-dispatch-fanout"` | `constants.rs` (`REASON_TRAIT_DISPATCH_FANOUT`) |
| Reason (suppressed) | `"trait-dispatch-suppressed"` | `constants.rs` (`REASON_TRAIT_DISPATCH_SUPPRESSED`) |

The edge kind (`TraitDispatchCall`) is the primary provenance signal — it is stable across
serializations and distinguishable in every query surface. The reason constants are the
human-readable companion for downstream consumers that want to string-match provenance.

---

## Synthesized vs. Directly Extracted Edges

| Edge type | Kind | Confidence | Tier | Source |
|-----------|------|------------|------|--------|
| Synthesized trait-dispatch caller | `TraitDispatchCall` | 0.70 | `Inferred` | Builder `maybe_add_trait_dispatch_call` |
| Directly extracted impl→trait | `Implements` | 0.85 | `Inferred` (below 0.9) | SCIP `Implementation` relationship |
| Directly extracted definition | `Defines` | 0.85 | `Inferred` (below 0.9) | SCIP `Definition` relationship |
| Directly extracted reference | `SymbolReference` | 0.90 | `Extracted` | SCIP occurrence |
| Directly extracted read | `Reads` | 0.85 | `Inferred` (below 0.9) | SCIP `ReadAccess` |
| Directly extracted write | `Writes` | 0.90 | `Extracted` | SCIP `WriteAccess` |

Key distinction: synthesized `TraitDispatchCall` edges sit at `0.70` — below both extracted SCIP
references (`0.90`) and structural `Implements`/`Defines` edges (`0.85`). This means they are
always in the `Inferred` tier and can be filtered out of default-threshold queries while still
appearing when a user explicitly lowers `min_confidence`.

---

## `min_confidence` Behavior

- **Default `impact` threshold:** `0.85` (`None → 0.85` in `shared::impact_bfs_with_policy`).
- Because the trait-dispatch floor (`0.70`) is **below** the default, callers who want
  trait-dispatch callers in the blast radius must pass an explicit lower threshold
  (e.g. `Some(0.70)` or `Some(0.0)`).
- **Directly extracted SCIP edges** (`Implements` at `0.85`, `Defines` at `0.85`, `SymbolReference`
  at `0.90`, etc.) are unaffected by this filter because their confidence values remain at or above
  the default.
- This is documented in the `RepoGraphOps::impact` trait doc comment in
  `server/crates/djinn-control-plane/src/bridge/graph_bridge.rs` (lines 93–109).

### Example

```text
# Default threshold (0.85): TraitDispatchCall edges (0.70) are filtered OUT
code_graph impact <symbol>                    # → blast radius excludes trait-dispatch callers

# Explicit threshold at floor: TraitDispatchCall edges are included
code_graph impact <symbol> --min-confidence 0.70   # → blast radius includes trait-dispatch callers
```

---

## Deployed / Manual UI Verification Checklist

> **⚠ Not a worker acceptance criterion.**
> Workers do not have production credentials, staging deployment access, or operator permissions.
> This checklist is for operators verifying the deployed behavior after a release.

- [ ] Deploy a build that includes the `qjo3` changes to a staging environment with a warmed v11
      graph artifact.
- [ ] Open `/code-graph` for the `djinnos/djinn` project.
- [ ] Search for `RuntimeOps::list_taskrun_jobs` and select it.
- [ ] Verify the **Dependents/Impact** section shows `reap_orphaned_taskrun_jobs` under **Calls**
      with confidence ~0.70.
- [ ] Verify the **Dependencies** section shows the concrete impl (`AppState::list_taskrun_jobs`)
      under **Implements** with confidence ~0.90.
- [ ] Repeat for `RepoGraphOps::context` and `SlotPoolOps::get_status`.
- [ ] Verify the `min_confidence` parameter on `code_graph impact` behaves correctly: with the
      default threshold, trait-dispatch callers are excluded; with `min_confidence ≤ 0.70`, they
      appear.

---

## Follow-Up Recommendations

1. **Per-language expansion:** The builder currently limits `maybe_add_trait_dispatch_call` to
   `file.language == "rust"`. TypeScript/Java interface dispatch and Python ABC dispatch should be
   evaluated in a future epic once the Rust validation is stable.

2. **Dynamic impl discovery:** The fan-out uses the static `trait_impl_index` built from SCIP
   `Implementation` relationships. Runtime-only impls (e.g. trait objects constructed from external
   crates) are not captured; a future enhancement could use call-graph analysis or type inference to
   widen the index.

3. **UI tier rendering:** The `SymbolDetailPanel` currently renders `confidence` as a numeric value.
   A future UX pass could badge `Inferred` vs `Extracted` edges visually so users understand why
   trait-dispatch callers have lower confidence than direct SCIP references.

4. **Agent prompt updates:** The agent-facing `code_graph` tool documentation in `djinn-agent` prompts
   should mention the `min_confidence` threshold behavior for trait-dispatch edges so workers know
   when to lower the threshold to include synthesized callers.

---

## Related

- **Reproduction corpus:** `server/src/mcp_bridge/graph_ops/tests/trait_dispatch_corpus.rs`
- **Corpus e2e tests:** `server/src/mcp_bridge/graph_ops/tests/trait_dispatch_corpus_e2e.rs`
- **Query regressions:** `server/src/mcp_bridge/graph_ops/tests/trait_dispatch_query.rs`
- **Impact regressions:** `server/src/mcp_bridge/graph_ops/tests/trait_dispatch_impact.rs`
- **Agent dispatch tests:** `server/crates/djinn-agent/src/extension/tests/code_graph_tests.rs`
- **UI component test:** `ui/src/components/codegraph/SymbolDetailPanel.test.tsx`
- **Graph builder (fan-out):** `server/crates/djinn-graph/src/repo_graph/builder.rs`
- **Constants (confidence/cap/reasons):** `server/crates/djinn-graph/src/repo_graph/constants.rs`
- **Edge tier classification:** `server/crates/djinn-graph/src/repo_graph/edge.rs`
- **Bridge impact doc comment:** `server/crates/djinn-control-plane/src/bridge/graph_bridge.rs`
- **Memory roadmaps:** `design/h1hn-roadmap`, `design/ggrm-roadmap`, `design/5wyo-roadmap`
