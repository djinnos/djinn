---
title: ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene
type: adr
tags: ["adr","planner","codebase-learning","memory-hygiene","patrol","architect","consolidation"]
---

# ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene

## Status

Proposed

Date: 2026-04-13

Related: [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]], [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]], [[ADR-051: Planner as Patrol and Architect as Consultant]], [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]

## Context

Djinn's knowledge extraction is purely **session-reactive** — it only learns from what happened during a task. This misses structural truths the codebase reveals directly: module boundaries, dependency patterns, subsystem roles, architectural hotspots. The system also lacks proactive memory hygiene — duplicate clusters grow, stale notes persist, orphan counts climb (currently 798), and no agent takes responsibility for cleanup.

### What already exists

**Patrol infrastructure** ([[ADR-051: Planner as Patrol and Architect as Consultant]]):
- Planner runs self-scheduling patrols (5-60 min interval)
- Planner already reviews board health during patrol
- Planner can spawn tasks: `planning`, `review`, `decomposition`
- Architect handles `spike` tasks as on-demand consultant
- Coordinator dispatches based on issue_type → role routing

**Code graph infrastructure**:
- SCIP-based `code_graph` with PageRank, structural weight, SCC detection
- `CanonicalGraphRefreshPlanner` detects staleness via pinned commit comparison
- Graph refreshes every 10 minutes when stale
- `diff` operation shows what changed since previous canonical graph
- `impact`, `neighbors`, `orphans`, `cycles`, `ranked` queries available

**Memory hygiene infrastructure**:
- `access_count` + `last_accessed` on every note
- Bayesian confidence with `STALE_CITATION` (0.3) and `CONTRADICTION` (0.1) signals
- `flag_orphan_notes()` — zero inbound links + not accessed in 30 days + access_count=0
- `NoteConsolidationRepository` — `likely_duplicate_clusters()`, `synthesize_cluster()`
- Coordinator hourly tick runs association pruning and basic consolidation
- `memory_health` MCP tool surfaces orphan count, broken links, stale note count

**What's missing**: No agent is responsible for interpreting these signals and taking action. The infrastructure produces data; nothing acts on it.

## Problem statement

1. **Codebase learning is passive** — Djinn only learns about code structure incidentally through task sessions. Major subsystems, boundaries, and architectural patterns go undocumented unless an agent happens to work on them.

2. **Memory hygiene is unowned** — Duplicate clusters, stale notes, orphans, and weak extractions accumulate without any agent taking responsibility for cleanup. The coordinator runs basic consolidation hourly but doesn't reason about what to consolidate or why.

Both problems have the same solution shape: give the Planner awareness of knowledge state and code structure during patrol, and let it spawn targeted tasks when it identifies gaps or hygiene needs.

## Decision

Extend the **Planner patrol** to include memory health and code structure awareness. The Planner becomes responsible for identifying knowledge gaps and hygiene needs, and spawns **spike** tasks (Architect) or **planning** tasks (self) to address them. No new agent roles. No new patrol infrastructure. This builds entirely on [[ADR-051: Planner as Patrol and Architect as Consultant]].

### 1. Enrich patrol context with memory and code signals

Add to the Planner's patrol context assembly (alongside existing board health data):

**Memory health signals** (from `memory_health` + new queries):
- Total note count, orphan count, broken link count
- Duplicate cluster count and largest cluster size
- Notes with zero access in 30+ days
- Notes with confidence below 0.3 (effectively stale/contradicted)
- Recent extraction quality metrics (from `ExtractionQuality` in session taxonomy)

**Code structure signals** (from `code_graph`):
- Recent graph diff summary (new/removed/changed modules since last patrol)
- High-PageRank symbols without scoped notes (knowledge gaps)
- Subsystem clusters with high code churn but low note coverage
- New modules that appeared since last patrol

**Coverage analysis**:
- Cross-reference `scope_paths` on existing notes with active code modules
- Identify code areas with dense notes vs areas with none
- Surface areas where notes exist but code has changed significantly (staleness candidates)

### 2. Planner spawns targeted knowledge tasks

Based on patrol context, the Planner can create these task types using existing dispatch infrastructure:

#### Spike tasks → Architect explores and documents

| Trigger | Task description |
|---------|-----------------|
| New module in graph with no scoped notes | "Explore and document {module}: purpose, boundaries, key interfaces, dependencies" |
| High-PageRank subsystem with no overview note | "Write subsystem overview for {subsystem}: role, API surface, invariants, connection to adjacent modules" |
| Major structural change in graph diff | "Investigate structural change in {area}: what changed, why, impact on existing patterns/pitfalls" |

The Architect reads code via `code_graph` queries and file reads, then writes subsystem overview notes, boundary notes, and architectural observations. These notes are higher quality than session-extracted knowledge because the Architect is specifically tasked with understanding structure, not incidentally observing it while fixing a bug.

#### Planning tasks → Planner consolidates and curates

| Trigger | Task description |
|---------|-----------------|
| Duplicate cluster with 3+ notes | "Consolidate {N} near-duplicate notes about {topic} into one strong canonical note" |
| Notes scoped to significantly changed code | "Review {N} notes scoped to {area} — code has changed since extraction, verify accuracy and update or archive" |
| Orphan count exceeds threshold | "Audit {N} oldest orphan notes — archive dead residue, repair broken links where possible, promote any still-valuable notes" |
| Weak extraction batch | "Review recent extractions with low quality scores — promote strong ones, archive weak ones" |

The Planner performs these tasks itself (planning issue_type, dispatched to Planner role). It reads notes, evaluates quality, calls `memory_edit` to merge/update, and calls `memory_delete` to archive dead content.

### 3. Codebase learning is Architect spikes, not a special loop

There is no hardcoded "codebase learning loop" in the coordinator. Instead:

1. The **graph refresh** happens automatically every 10 minutes (existing `CanonicalGraphRefreshPlanner`)
2. The **Planner patrol** sees the graph diff in its context
3. When the Planner identifies a knowledge gap (undocumented subsystem, missing boundary note), it **creates a spike task**
4. The **Architect** picks up the spike, explores the code, writes durable notes
5. Those notes go to the task's knowledge branch (ADR-055), then merge via quality gate

This is organic and self-regulating:
- If the board is busy with real work, knowledge tasks get lower priority (natural backpressure)
- The Planner decides when exploration matters based on the actual state of the KB and codebase
- No waste — spikes only fire for genuine gaps, not on a fixed schedule
- The Architect's deep code reasoning is better suited to structural understanding than a batch extraction job

### 4. Freshness decay driven by code changes

When the canonical graph refreshes and the diff shows changed symbols:

1. Coordinator queries notes with `scope_paths` intersecting changed file paths
2. Apply `STALE_CITATION` Bayesian signal (0.3) to reduce confidence
3. Add `review_needed` tag to affected notes
4. Planner sees these in next patrol context as "notes needing review"
5. Planner spawns review/planning tasks to verify and update or archive

This uses existing infrastructure:
- Graph diff (exists in `code_graph`)
- `scope_paths` on notes (exists, populated by extraction)
- Bayesian confidence signals (exists, `STALE_CITATION = 0.3`)
- Planner patrol (exists, ADR-051)

The new work is the **glue**: a function in the coordinator that, after graph refresh, cross-references the diff with note scope_paths and applies the staleness signal.

### 5. Memory health in patrol prompt

The Planner's patrol prompt template gains a new section:

```
## Memory Health

Total notes: {total}
Orphan notes: {orphans} ({orphan_pct}%)
Broken links: {broken_links}
Duplicate clusters (3+ notes): {dup_clusters}
Notes not accessed in 30+ days: {stale_count}
Notes with confidence < 0.3: {low_confidence_count}
Recent extraction quality: {extracted}/{written} written, {novelty_skipped} deduped

## Code Structure Changes (since last patrol)

New modules: {new_modules}
Removed modules: {removed_modules}
High-churn areas: {high_churn}
Undocumented high-PageRank symbols: {undocumented_hotspots}

## Knowledge Coverage Gaps

Areas with code but no notes: {uncovered_areas}
Areas with notes but changed code: {stale_areas}
```

The Planner interprets these signals and decides whether to spawn tasks. It might decide that 5 orphans aren't worth a task but 50 are. It might decide a new utility module doesn't need documentation but a new core subsystem does. This judgment is the Planner's job — the system surfaces signals, the agent makes decisions.

### 6. Guard rails

**Task budget**: The Planner should not flood the board with hygiene tasks. Guard with:
- Maximum N knowledge tasks per patrol (configurable, default 2)
- Knowledge tasks are lower priority than feature/bug work
- Don't create hygiene tasks if the board already has unclaimed ones

**Quality over quantity**: Architect spikes should produce fewer, stronger notes — subsystem overviews and boundary docs, not sprayed patterns/pitfalls/cases. The Architect prompt should emphasize structural understanding over individual observations.

**No circular extraction**: Notes written by Architect spikes or Planner cleanup tasks go through the same quality gate (ADR-054) and branching flow (ADR-055) as any other task. No special bypass.

## Alternatives considered

### A. Hardcoded learning loop in coordinator
Rejected. A fixed-interval extraction job is wasteful (runs when nothing changed) and inflexible (can't prioritize based on board state). The Planner-driven approach is organic and self-regulating.

### B. Dedicated "Curator" agent role
Rejected. Adds a new role, new dispatch rules, new prompt, new slot allocation. The Planner and Architect already have the right capabilities — the Planner for evaluation/consolidation, the Architect for deep code exploration. Adding a role is premature when existing roles can absorb the work.

### C. Human-triggered cleanup only
Rejected. The evidence shows humans don't clean up memory — 798 orphans accumulated without intervention. The system must be proactive.

### D. Automated consolidation without agent judgment
Partially useful (and the coordinator already does basic consolidation hourly), but merging notes requires judgment about what the canonical version should say. An LLM agent making deliberate consolidation decisions produces better results than a heuristic merge.

## Consequences

### Positive
- Codebase knowledge grows organically as the Planner identifies gaps
- Memory hygiene becomes a continuous, agent-driven process
- No new agent roles or dispatch infrastructure needed
- Self-regulating: knowledge tasks compete for slots naturally
- Architect produces high-quality structural documentation (better than incidental session extraction)
- Freshness decay is code-change-driven, not just time-based
- Planner develops awareness of knowledge state alongside board state

### Negative
- Planner patrol becomes more complex (larger context, more decision surface)
- Architect spikes consume slots that could serve feature work
- Risk of the Planner over-generating hygiene tasks (mitigated by task budget guard)
- Coverage analysis queries add latency to patrol context assembly
- Depends on graph diff quality (SCIP indexer coverage, refresh reliability)

## Migration / rollout

### Phase 1 — Memory health in patrol context
- Add memory health summary to Planner patrol prompt
- No new task creation yet — just visibility
- Validate that the Planner notices and comments on hygiene issues

### Phase 2 — Planner spawns hygiene tasks
- Enable Planner to create planning tasks for consolidation and orphan cleanup
- Add task budget guard (max 2 knowledge tasks per patrol)
- Test with manual patrol triggers

### Phase 3 — Code structure awareness
- Add graph diff summary to patrol context
- Add coverage gap analysis (scope_paths vs active modules)
- Enable Planner to create spike tasks for undocumented subsystems

### Phase 4 — Freshness decay
- Wire graph diff → scope_path intersection → `STALE_CITATION` signal
- Add `review_needed` tagging for affected notes
- Planner sees stale notes in patrol and spawns review tasks

### Phase 5 — Tuning
- Adjust task budget based on board throughput
- Tune coverage gap thresholds (what counts as "undocumented")
- Adjust staleness decay aggressiveness
- Monitor knowledge quality over time

## Relations

- [[ADR-051: Planner as Patrol and Architect as Consultant]]
- [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]]
- [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]]
- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]