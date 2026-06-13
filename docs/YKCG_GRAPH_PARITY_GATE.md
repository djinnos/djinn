# ykcg graph parity gate runbook

This runbook defines the standing parity discipline for ykcg Route, Process, Tool, and future graph-ingestion waves. The permanent artifact is the reusable parity API/reporting path in `djinn-graph`; per-extractor shadow flags and dual-build seams are temporary rollout scaffolding only.

## Gate invariant

Every extractor rollout compares two graph artifacts:

1. **Baseline graph**: the canonical graph immediately before the extractor or ingestion change under test runs.
2. **Live graph**: the canonical graph after the extractor or ingestion change runs.

Run the comparison through the ykcg parity adapter (`assert_ykcg_extractor_graph_parity` or `assert_ykcg_extractor_artifact_blob_parity`). The adapter uses the core graph-parity diff and then classifies only explicitly allowlisted additions as intentional.

The gate must fail on:

- file additions/removals in the compared graph artifacts;
- node removals of any kind;
- edge removals of any kind;
- added node kinds that are not in the extractor allowlist;
- added edge kinds that are not in the extractor allowlist;
- community additions/removals; and
- community membership drift, except for membership additions whose node UID belongs to an allowlisted newly added node kind.

The gate must emit or attach the structured parity report (`render_for_ci()` output or the serialized report shape) so PR reviewers and CI logs show counts by kind plus bounded added/removed samples.

## Standing extractor allowlists

Use the narrowest allowlist for the extractor being rolled out:

| Extractor/change | Allowed added node kinds | Allowed added edge kinds |
| --- | --- | --- |
| Route extraction | `Route` | `HandlesRoute`, `Fetches` |
| Process enrichment | `Process` | `StepInProcess` |
| Tool extraction / tool-surface ingestion | `Tool` | none today |

Tool-specific edge kinds are not pre-approved. When a future Tool extractor introduces a concrete Tool edge kind, add that edge kind to the Tool allowlist in the same change that introduces the model/test coverage for the edge. Until then, Tool parity coverage should prove only `Tool` node additions are allowed and that unallowlisted Tool additions fail.

All other core graph/community drift is a parity failure. Do not widen an allowlist to hide unrelated SCIP, resolver, community, file, or graph-op changes.

## Temporary shadow flag deletion rule

Per-extractor environment flags, config seams, and dual-build paths exist only to make a rollout observable while the extractor is being adopted. Delete them once the extractor is fully shipped and the live extractor path is the canonical behavior.

After shipment, keep only:

- the reusable ykcg parity helper/API;
- fixture tests that prove the allowlist/reporting behavior; and
- this runbook or equivalent standing-gate documentation.

Do not leave a long-lived dual pipeline, permanent shadow mode, or alternate Tool/source extractor path behind.

## Future ingestion and resolver changes

Future graph ingestion or resolver changes should use the same parity contract even when they are not named Route/Process/Tool extractors:

1. Build or load the old/baseline graph artifact and the new/live graph artifact at the same repository revision.
2. Compare artifacts through `assert_ykcg_extractor_artifact_blob_parity` when serialized blobs are available, or through `assert_ykcg_extractor_graph_parity` for in-memory fixtures.
3. Allowlist only intentionally new node/edge kinds introduced by the change.
4. Treat all pre-existing node kinds, edge kinds, file populations, and community populations as strict parity invariants.
5. Attach or emit the structured parity diff in CI/PR logs on both pass and fail paths. Passing reports should show the allowed additions; failing reports should include the bounded core diff samples that make the drift actionable.

If the change is expected to alter existing graph semantics rather than add a new kind, do not bypass the gate. Split the migration so the additive model lands first with a narrow allowlist, then make any intentional core-semantic change explicit in its own reviewable task with before/after evidence.

## e148 incremental == full equivalence

The e148 incremental-graph work should validate incremental rebuilds by comparing the incrementally updated graph artifact against a full rebuild artifact through the parity API/report:

1. Produce the **incremental** artifact from the changed-file/update path.
2. Produce the **full** artifact from a clean full graph rebuild for the same commit and indexer inputs.
3. Run the graph parity comparison over those artifacts.
4. For ordinary incremental==full checks, use an empty allowlist: the artifacts should be identical after normalization.
5. If the incremental work also introduces a new graph kind, allow only that intentionally new kind and document why the full rebuild produces the same addition.
6. Attach/emit the structured diff report in CI/PR logs so failures show per-kind count deltas and bounded added/removed samples.

This makes e148 failures reviewable as graph-artifact parity failures rather than opaque incremental cache mismatches.
