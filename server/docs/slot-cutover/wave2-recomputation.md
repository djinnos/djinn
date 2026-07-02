# Slot Cut-over Final Verification — Wave 2 Recomputation (Task q2bk)

> Generated: 2026-07-02  
> Task: `019f1e5a-66db-7b70-b07e-34e1c1e41e40` — Recompute final slot line-count proof and closeout validation after reduction wave

This section records the post-reduction-wave state after helper tasks (b7pe, qicv, 2sy0), lifecycle thinning (560g), and supervisor dispatch consolidation (t9i0) have landed. It is appended to the existing `final-verification.md` artifact so the next planner can decide whether epic `0ecv` can be closed or whether a further wave is required.

## 1. Scope recomputed

The same Rust file scopes as the foundation baseline and prior proof:

- `server/crates/djinn-agent/src/actors/slot`
- `server/crates/djinn-slot/src`
- Combined total

Method: `find <tree> -name '*.rs' -type f -print0 | xargs -0 cat | wc -l` from `server/`.

## 2. Final line counts

### 2.1 Baseline (foundation task q7y6 / abi6 proof)

| Tree | Baseline lines |
|---|---|
| `server/crates/djinn-agent/src/actors/slot` | **24,490** |
| `server/crates/djinn-slot/src` | **20,822** |
| **Combined** | **45,312** |

### 2.2 Prior proof after Wave 1 / pre-Wave-2 (task abi6, 2026-07-01)

| Tree | Lines |
|---|---|
| `server/crates/djinn-agent/src/actors/slot` | **10,705** |
| `server/crates/djinn-slot/src` | **26,541** |
| **Combined** | **37,246** |

### 2.3 Current counts after Wave 2 reduction tasks (2026-07-02)

```bash
$ find server/crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
9396

$ find server/crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
28242

$ find server/crates/djinn-agent/src/actors/slot server/crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
37638
```

| Tree | Baseline | Current | Delta vs baseline |
|---|---|---:|---:|
| `server/crates/djinn-agent/src/actors/slot` | 24,490 | **9,396** | **−15,094** (−62%) |
| `server/crates/djinn-slot/src` | 20,822 | **28,242** | +7,420 (+36%) |
| **Combined** | **45,312** | **37,638** | **−7,674** (−17%) |

### 2.4 Wave 2 incremental reduction (abi6 → q2bk)

| Metric | Value |
|---|---|
| Agent slot reduction | 10,705 → **9,396** = **−1,309 lines** |
| djinn-slot growth | 26,541 → **28,242** = **+1,701 lines** |
| Combined net change | 37,246 → **37,638** = **+392 lines** |

The wave-2 reduction tasks removed or delegated agent-side helper and lifecycle code, but `djinn-slot` grew as it absorbed canonical dispatch orchestration (`dispatch_orchestrator.rs`, `dispatch_utils.rs`) and lifecycle implementations (`task_classifier.rs`, `retry.rs`). The net effect was a small combined increase relative to the abi6 checkpoint.

### 2.5 Line-count verdict

**The combined total dropped by 7,674 lines (45,312 → 37,638). The required 15,000-line reduction is NOT met.** The shortfall is **7,326 lines**.

This is larger than the 6,934-line shortfall recorded at abi6 because the canonical slot crate grew more than the agent crate shrank during Wave 2.

## 3. What changed in Wave 2

### 3.1 Helper consolidation (b7pe, qicv, 2sy0)

- Agent-side `helpers/code_context.rs` and `helpers/reviewer_diff.rs` deleted; logic delegated to canonical `djinn-slot` helpers via thin `AgentContext → SlotContext` adapters in `helpers/mod.rs`.
- Agent-side `helpers/feedback.rs` reduced to a thin adapter layer; context-free functions re-exported from `djinn-slot`, `AgentContext`-bound functions wrapped.
- Agent-side `helpers/tests.rs` reduced to a documentation stub; behavioral tests moved to canonical `djinn-slot/src/helpers/tests.rs`.
- `provider_resolution.rs` retained as host-only credential/OAuth management.

### 3.2 Lifecycle thinning (560g)

- `lifecycle/task_classifier.rs` → 5-line facade; canonical implementation moved to `djinn-slot`.
- `lifecycle/retry.rs` → 6-line facade; canonical implementation moved to `djinn-slot`.
- Remaining lifecycle files (`prompt_context.rs`, `mcp_resolve.rs`, `role_overrides.rs`, `setup.rs`, `teardown.rs`, `model_resolution.rs`) kept as host-only adapters due to agent-only types and public caller boundaries (documented in the Wave 2 lifecycle section above).

### 3.3 Supervisor dispatch consolidation (t9i0)

- `djinn-slot/src/dispatch_orchestrator.rs` introduced as the canonical dispatch lifecycle (load task → resolve context → build spec → credentials → runtime → prepare → stream → teardown → post-dispatch bookkeeping).
- `djinn-slot/src/dispatch_utils.rs` updated to support host callback lifetime patterns.
- `djinn-agent/src/actors/slot/supervisor_runner.rs` remains host-only dispatch logic. Contrary to the original task target, the large dispatch body was not thinned to a tiny adapter in this session; the file is still **1,071 lines** of `AgentContext`-specific runtime selection, credential resolution, pre-session liveness, breaker feedback, and post-run bookkeeping. The canonical orchestrator exists in `djinn-slot`, but the agent side still contains the concrete host wiring because the `SlotHostCallbacks` path continues to route through it.

## 4. Remaining agent slot breakdown

Commands used:

```bash
# Files importing djinn_slot (thin shims)
grep -rl 'djinn_slot::' server/crates/djinn-agent/src/actors/slot/ --include='*.rs'

# Files NOT importing djinn_slot (host-only or tests)
for f in $(find server/crates/djinn-agent/src/actors/slot -name '*.rs' -type f); do
  if ! grep -q 'djinn_slot' "$f"; then echo "$(wc -l < "$f") $f"; fi
done
```

| Category | Files | Lines | Notes |
|---|---:|---:|---|
| **Thin shims / re-exports importing `djinn_slot`** | 12 | ~1,650 | `mod.rs`, `actor.rs`, `commands.rs`, `finalize_handlers.rs`, `host_callbacks.rs`, `llm_extraction.rs`, `memory_enrichment.rs`, `pool/handle.rs`, `pool/types.rs`, `helpers/provider_resolution.rs`, `reply_loop/mod.rs`, `session_extraction.rs` |
| **Host-only implementation** | 15 | ~5,300 | `supervisor_runner.rs` (1,071), lifecycle files (2,681), `host_callbacks.rs`, `actor.rs`, `helpers/mod.rs`, `pool/handle.rs`, etc. |
| **Host/facade tests** | 3 | ~2,446 | `lifecycle/prompt_context_tests.rs`, `lifecycle/ci_directive_tests.rs`, `helpers/tests.rs` |
| **Total** | **30** | **9,396** | |

The largest remaining host-only files are:

| File | Lines | Why it remains |
|---|---|---|
| `supervisor_runner.rs` | 1,071 | Concrete `AgentContext`-specific runtime selection, credential resolution, pre-session liveness, breaker feedback, and post-run bookkeeping. The canonical orchestrator lives in `djinn-slot`, but the host callback seam still routes through this file. |
| `lifecycle/prompt_context.rs` | 796 | Depends on `AgentContext` repositories, git worktree, memory context. |
| `lifecycle/mcp_resolve.rs` | 491 | Depends on agent `ResolvedSkill`, `McpToolRegistry`, native skill assets. |
| `lifecycle/role_overrides.rs` | 466 | Returns `Arc<dyn AgentRole>`; maps agent `AgentType` values. |
| `lifecycle/teardown.rs` | 225 | Coordinates task transitions, coordinator triggers, background work. |
| `lifecycle/setup.rs` | 145 | Runs commands through agent command stack. |
| `lifecycle/model_resolution.rs` | 187 | Exposes agent `ProviderCredential` for worker secret serialization. |

## 5. Duplicate-behavior sweep refresh

All remaining agent slot files are classified as one of:

1. **Host-only** — depends on `AgentContext` and has no duplicate in `djinn-slot`.
2. **Thin shim / re-export** — delegates to `djinn-slot` via `pub use` or small `AgentContext → SlotContext` adapters.
3. **Host/facade tests** — cover agent-only behavior or adapter compatibility.

No agent slot file contains an independent parallel implementation of behavior that exists in `djinn-slot`. The `supervisor_runner.rs` dispatch logic is still a host-only concrete implementation, but its canonical orchestration counterpart is now in `djinn-slot/src/dispatch_orchestrator.rs`; the two are not duplicates — one is the generic lifecycle, the other is the agent-specific host wiring that feeds it.

## 6. Closeout validation

Commands run from `server/`:

| Command | Outcome |
|---|---|
| `cargo fmt --check` | ✅ Passed |
| `cargo test -p djinn-slot --all-features --no-run` | ✅ Compiled successfully |
| `cargo test -p djinn-agent --all-features --no-run` | ✅ Compiled successfully |
| `cargo test -p djinn-slot --all-features --lib reply_loop::budget::tests` | ✅ 4 passed, 0 failed |

The full workspace `cargo test --workspace --all-features --no-run` was not run in-session per worker rules against workspace-wide commands; package-scoped no-run compilation succeeded for both slot crates. Automated post-session verification remains responsible for the workspace-wide gate.

All DB-dependent tests continue to fail on `ConnectionRefused` in this sandbox; no tests were disabled or weakened.

## 7. Acceptance-criteria status

| Criterion | Status | Detail |
|---|---|---|
| Exact line-count commands and outputs recorded | ✅ | §2 documents commands and outputs |
| Combined line count compared to 45,312 baseline | ✅ | 37,638 (−7,674) |
| 15,000-line reduction met | ❌ **Not met** | Shortfall of 7,326 lines |
| Duplicate-behavior sweep refreshed | ✅ | §4–5 classifies remaining agent files |
| Closeout validation run from `server/` | ✅ | fmt + package no-run + focused non-DB tests |
| No broad implementation refactor introduced | ✅ | Only documentation artifact updated by this task |

## 8. Planner follow-up

The 15,000-line reduction target remains unmet. The remaining safe/unsafe candidates are the same as after abi6, with the addition of the canonical dispatch orchestrator now living in `djinn-slot`:

- **Safe but requires caller migration:**
  - Convert remaining agent lifecycle helpers (`prompt_context.rs`, `mcp_resolve.rs`, `role_overrides.rs`, `setup.rs`, `teardown.rs`, `model_resolution.rs`) to use `SlotContext` + host callbacks, then delete agent copies. This is the largest remaining reduction (~2,681 lines in lifecycle alone).
  - Finish collapsing `supervisor_runner.rs` so the agent side is a pure callback adapter calling the canonical `djinn-slot` dispatch orchestrator. This could remove hundreds of lines of host-only dispatch wiring.
  - Consolidate near-duplicate `feedback.rs` / `code_context.rs` / `reviewer_diff.rs` parameterization by migrating remaining callers to `djinn_slot::helpers::*` directly.

- **Unsafe without impact analysis:**
  - Removing any public `djinn_agent::actors::slot::*` re-export or changing a signature used by workspace callers. The destructive-change impact tool was not available in this wave; such moves require a dedicated impact-checked pass.

Because the combined line count actually increased slightly from the abi6 checkpoint (37,246 → 37,638), the next planner should either:

1. Accept the remaining shortfall and close `0ecv` with a documented miss, or
2. Launch a Wave 3 specifically targeting the remaining lifecycle and supervisor host-wiring files with a pre-approved public-API impact map.

This task does not implement further refactors; it only records the final state and blockers.
