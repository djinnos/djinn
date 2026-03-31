# ADR-045: SSE Event Batching and Knowledge Base Housekeeping — Roadmap

## Summary

ADR-045 is in its residual hardening wave. Wave 1 landed the core implementation across all four sub-phases, and 45d is complete. The remaining work is narrow follow-up coverage and integration hardening in 45a, 45b, and 45c that surfaced after the main merges were reviewed together.

## ADR-045 scope status by sub-phase

- **45a — SSE BatchAccumulator + client debounce:** core batching/debounce work landed in Wave 1, but one residual mixed-traffic hardening regression remains.
- **45b — Content hash dedup gate + migration:** core write-path dedup and repair semantics landed in Wave 1, but one residual initialization/backfill proof remains for legacy note rows.
- **45c — Background HousekeepingWorker:** fixture seams, observability, and deterministic repair pieces landed in Wave 1, but one residual server tick integration coverage gap remains.
- **45d — Confidence filtering + session consolidation + Architect patrol extension:** **complete**.

## Wave 1 outcomes

Wave 1 successfully landed the main ADR-045 implementation and supporting seams:

- `icvf` — hardened SSE batching coverage and client invalidation debounce integration.
- `ubvz` — landed the content-hash write-path dedup behavior on fresh main.
- `srh2` — completed confidence filtering, session-scoped consolidation, and Architect memory-health prompts.
- `tgsq` — added housekeeping observability/config helpers in `server/src/housekeeping.rs`.
- `dx64` — added deterministic housekeeping repository fixtures for multi-project count assertions.
- `hrji` — exposed the deterministic broken-wikilink fixture seam used by housekeeping tick tests.

Additional follow-on Wave 1 closures that support the residual hardening path:

- `xhct` — added a focused SSE mixed-traffic batching integration regression.
- `mr6c` — added a deterministic legacy SQLite fixture seam for note content-hash migration coverage.
- `0u7f` — locked down `rebuild_missing_content_hashes` repair semantics without duplicate creation.
- `2eol`, `fk3c`, `b3h1`, `386n`, `5q6b`, and `d0zc` — iterated the housekeeping deterministic fixture and server tick test surface until the remaining gap was narrowed to one final focused integration assertion.

## Residual gaps after merges

Post-merge review of the completed Wave 1 work showed the epic is not yet fully done even though the main architectural pieces landed:

1. **45a residual:** prove the SSE stream behavior under one mixed-traffic scenario that combines immediate structural events with coalesced entity updates and throttled session traffic, ensuring the ordering and boundedness expected by the desktop client remain intact.
2. **45b residual:** prove `Database::ensure_initialized()` backfills legacy pre-content-hash note rows through the supported initialization path using the deterministic legacy fixture seam.
3. **45c residual:** prove the actual server housekeeping tick entrypoint exercises the deterministic fixture seams end-to-end for the targeted multi-project and broken-wikilink repair scenarios.

These are hardening/coverage tasks, not a re-open of the already landed ADR decisions.

## Wave 2 decomposition

Wave 2 is intentionally limited to one focused worker task per unfinished sub-phase:

- **45a:** `nax1` — **Add SSE mixed-traffic batching regression coverage for structural plus throttled event bursts**
  - Goal: cover immediate structural events, coalesced task updates, and throttled session traffic in a single regression using the existing EventBus/SSE test surface.
- **45b:** `b534` — **Prove initialization backfills legacy note content hashes via Database::ensure_initialized**
  - Goal: use the deterministic legacy SQLite fixture seam to prove old rows gain normalized hashes after initialization, with only a minimal production fix if current main still misses the backfill.
- **45c:** `ixz0` — **Finish server housekeeping tick integration coverage with deterministic fixture seam**
  - Goal: exercise the real housekeeping tick/runner entrypoint and assert deterministic totals or repairs across the targeted scenarios.

## Sequencing notes

- `45d` requires no further work unless a new defect is discovered outside this epic.
- `nax1`, `b534`, and `ixz0` are independent residual hardening tasks and can dispatch in parallel.
- Existing architect patrol feedback about singleton-note broken ADR wikilinks in `brief`/`roadmap` remains a separate metadata-drift concern and does not change the scoped ADR-045 Wave 2 decomposition above.

## Exit criteria for epic closure

The epic can close once the three Wave 2 residual tasks (`nax1`, `b534`, `ixz0`) are complete and no further ADR-045 gaps remain in SSE batching, content-hash backfill coverage, or server housekeeping tick integration coverage.
