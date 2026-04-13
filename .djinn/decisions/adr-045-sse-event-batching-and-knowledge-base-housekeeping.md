---
title: ADR-045: SSE Event Batching and Knowledge Base Housekeeping
type: 
tags: ["sse","memory","cleanup","dedup","performance"]
---


# ADR-045: SSE Event Batching and Knowledge Base Housekeeping

## Status

Accepted

## Context

Two operational gaps surfaced during a comparative analysis with the Ruflo (Claude Flow) reference architecture:

### SSE Event Volume

Every repository write emits an SSE event immediately via the broadcast channel (1024-capacity). During active execution, high-frequency events — `session.token_update` (per-stream-chunk), `session.message` (per-delta), and rapid `task.updated` bursts — cause:

- **Client-side thrash**: Zustand store updates + TanStack Query invalidations cascade into unnecessary React re-renders.
- **Lagged notifications**: If a client falls behind the broadcast buffer, it receives a `"lagged"` event and must refetch the entire board state — an expensive operation that compounds the problem.
- **Network saturation**: Multiple concurrent sessions multiply the event rate linearly.

The desktop frontend has no throttling, debouncing, or coalescing on either side.

### Knowledge Base Drift

The memory system has strong diagnostics (health reports, orphan detection, broken link detection, duplicate clustering) but almost no automated cleanup:

- **Duplicate notes accumulate** — `dedup_candidates()` runs at write time but only warns; the write proceeds regardless. LLM extraction creates notes at confidence 0.5 with novelty checks, but structurally similar notes still slip through.
- **Broken wikilinks persist** — detected by `memory_health` but never auto-repaired.
- **Stale associations linger** — `prune_associations()` and `prune_old_associations()` exist in the repository but are never called.
- **Contradicted notes pollute context** — notes with confidence < 0.1 (CONTRADICTION signal) still appear in `memory_build_context` results, ranked lower but not excluded.
- **No content-hash dedup** — identical content can be stored multiple times across separate writes.

## Decision

### 1. Server-Side SSE Event Batching

Add a `BatchAccumulator` between the EventBus and the SSE stream handler. Events are classified into three tiers:

| Tier | Behavior | Events |
|------|----------|--------|
| **Immediate** | Bypass batching, send instantly | `task.created`, `task.deleted`, `epic.created`, `epic.deleted`, `session.dispatched`, `session.ended`, `lifecycle.step` |
| **Coalesced** | Keep only the latest per entity ID within the flush window | `task.updated`, `epic.updated`, `agent.updated`, `project.updated` |
| **Throttled** | Rate-limited to max N per interval | `session.message` (max 1/50ms), `session.token_update` (max 1/500ms), `verification.step` (max 1/200ms) |

**Flush interval**: 100ms (imperceptible to humans, significant reduction in event volume).

**Implementation**: A per-SSE-subscriber tokio task that receives from the broadcast channel, accumulates into a `HashMap<(EventType, EntityId), Event>` for coalesced events, and flushes on a 100ms `tokio::time::interval`. Immediate events are forwarded without buffering. Throttled events use a per-type `Instant` tracker to enforce minimum intervals.

**Client-side complement**: TanStack Query invalidations should be debounced at 150ms to absorb batched events arriving in a single flush.

### 2. Write-Time Content Hash Dedup Gate

Add a `content_hash` TEXT column to the `notes` table. On every `memory_write`:

1. Compute SHA-256 of normalized content (trimmed whitespace, normalized line endings).
2. If an exact hash match exists in the same project: return the existing note instead of creating a duplicate. Include a `deduplicated: true` flag in the response.
3. If no exact match: proceed with the existing `dedup_candidates()` BM25 check and include candidates in the response (as today).

Migration: backfill existing notes with computed hashes.

### 3. Background Housekeeping Timer

A periodic tokio task (`HousekeepingWorker`) running on a configurable interval (default: 1 hour). All operations are deterministic SQL — no LLM calls required.

**Operations per tick:**

| Operation | Logic | Source |
|-----------|-------|--------|
| **Prune stale associations** | Delete where `weight < 0.05 AND age > 90 days` | Existing `prune_old_associations()` |
| **Flag orphan notes** | Notes with zero inbound links, zero access in 30+ days, not singleton types | Existing `memory_orphans` query |
| **Auto-fix broken wikilinks** | For each broken `[[target]]`, FTS-search for best match by title; if BM25 > threshold, update the source note's content | New — uses existing FTS |
| **Rebuild stale content hashes** | Notes where `content_hash IS NULL` | New — backfill migration supplement |

Results are logged as structured tracing events. No notes are deleted automatically — orphans are flagged (e.g., tagged `orphan`) for human or agent review.

### 4. Confidence-Based Context Filtering

Add a `min_confidence` parameter to `memory_build_context` (default: `0.1`).

- Notes with confidence below the threshold are **excluded entirely** from context results, not just ranked lower.
- Notes with `STALE_CITATION` signal (superseded) are annotated with `[superseded]` in their context representation when included.
- Notes with `CONTRADICTION` signal are excluded by default (confidence typically < 0.1).

### 5. Extended Session Extraction — Consolidation Pass

After the existing LLM extraction step (which creates new notes), add a consolidation pass:

1. Query `likely_duplicate_clusters()` scoped to notes written during this session.
2. For each cluster with 2+ members: call the memory provider LLM to select the canonical note and merge content.
3. Call `create_canonical_consolidated_note()` for each resolved cluster.
4. Delete or archive the non-canonical duplicates.

This runs in the existing post-session background task — no new agent role needed.

### 6. Architect Patrol — Memory Health Extension

Extend the Architect patrol prompt to include a memory health section:

- Surface `memory_health` report (orphan count, broken link count, stale count).
- If contradictions exist (notes with confidence < 0.1 and CONTRADICTION association), include them for the Architect to resolve — pick the canonical version, update references, and deprecate the other.
- The Architect already has read access to the knowledge base; this adds a "check memory health" step to its existing board review.

## Consequences

### Positive

- **SSE batching reduces event volume by ~10x** during active execution without perceptible UI lag (100ms flush).
- **Content hash prevents exact duplicates** at write time — zero-cost O(1) check.
- **Background housekeeping** runs deterministic cleanup without consuming LLM tokens.
- **Confidence filtering** prevents contradicted/superseded notes from polluting agent context.
- **Session consolidation** cleans up the most common source of duplicates (LLM extraction) at the point of creation.
- **Architect patrol extension** gives the only role with board-wide context the ability to curate knowledge — no new role needed.

### Negative

- **100ms SSE latency** is a tradeoff. Real-time-sensitive UIs (e.g., token counters) may feel slightly less responsive. Mitigated by the 50ms throttle on `session.message` being faster than human perception.
- **Content hash is exact-match only** — paraphrased duplicates still require BM25/LLM detection. This is acceptable; the hash handles the cheap case.
- **Broken link auto-fix** could make incorrect corrections if FTS returns a false positive. Mitigated by requiring a high BM25 threshold and logging all corrections.
- **Consolidation during extraction** adds LLM cost to post-session work. Mitigated by scoping to same-session notes only (small cluster size).

## Alternatives Considered

### Dedicated "Librarian" Agent Role

A new agent role solely for knowledge curation. Rejected: the volume of notes doesn't justify a dedicated role. Cleanup is better distributed across existing touchpoints (write-time, post-session, patrol, background timer).

### Client-Side-Only Batching

Debounce/throttle only on the desktop. Rejected: doesn't reduce server broadcast pressure, and every new client would need to re-implement the same logic.

### Reinforcement Learning for Router-Based Dedup

Use RL to learn which notes are duplicates over time. Rejected: premature complexity. Rule-based dedup (hash + BM25 + LLM novelty check) is more predictable and debuggable at current scale.

### Vector Embedding Similarity for Dedup

Use cosine similarity on embeddings instead of BM25 for duplicate detection. Deferred to Phase 17c/d (cognitive memory) when embedding infrastructure is in place. BM25 is sufficient for structural similarity detection today.

## Relations

- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]] — confidence scoring and association learning foundations
- [[ADR-034: Agent Role Hierarchy — Architect Patrol, Task Types, and Escalation]] — Architect patrol extension point
- [[decisions/adr-036-structured-session-finalization-finalize-tools-and-forced-tool-choice|ADR-036: Structured Session Finalization — Finalize Tools and Forced Tool Choice]] — session extraction pipeline extended
- [[ADR-042: DB-Only Knowledge Extraction, Consolidation, and Task Routing Fixes]] — consolidation logic reused
- [[Roadmap]] — new phase for this work
