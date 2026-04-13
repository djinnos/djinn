---
title: Repository Understanding and Memory Freshness Upgrade Path
type: 
tags: ["adr","memory","code-graph","scip","state-of-the-art","proposal"]
---


# Proposal: Repository Understanding and Memory Freshness Upgrade Path

## Status
Proposal

## Context
Djinn already has a strong base:
- SCIP-backed repository intelligence via [[ADR-043 Repository Map — SCIP-Powered Structural Context for Agent Sessions]]
- architect/chat graph query direction via [[ADR-050 Architect/Chat Code-Graph Consolidation, Canonical SCIP Indexing, and Graph Query Extensions]]
- semantic memory search via [[ADR-053 Semantic Memory Search — Candle Embeddings with sqlite-vec]]
- cognitive-memory behaviors already established by [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]

This proposal therefore assumes **ADR-023 is the conceptual baseline** and **ADR-053 is the current implementation wave**. The main question is no longer whether Djinn should have semantic retrieval or cognitive memory architecture; it is whether the **memory artifact model itself** is expressive enough for agents and humans to keep knowledge fresh and actionable.

A fresh external survey suggests the current state of the art is not a single technique, but a layered stack:
1. structural code intelligence
2. semantic retrieval
3. persistent memory abstractions
4. incremental freshness / background refresh
5. task- and role-scoped context shaping
6. explicit rules / living specs / operational memory
7. evaluation loops for memory quality and retrieval usefulness

## External findings

### Augment Code
- Emphasizes semantic dependency understanding and relevance over raw context size.
- Uses a context engine for large codebases and multi-agent work on top of a shared “living spec”.
- Strong idea: mutable shared execution spec that active agents read/write.
- Weakness: much of the public detail is marketing-heavy rather than protocol-level.

### Letta / MemGPT
- Strongest public conceptual model for agent memory hierarchy.
- Distinguishes always-in-context memory blocks from archival / filesystem / external retrieval.
- Explicit lesson: memory is context management, not just vector search.
- Stronger recent direction: git-backed/context-repository style persistent memory and benchmarked memory management.

### Sourcegraph / Cody / Deep Search
- Strongest public implementation of compiler-accurate repo understanding at enterprise scale.
- SCIP remains a best-in-class backbone for precise navigation and cross-repo semantic edges.
- Important pattern: combine exact search, semantic understanding, and fallback modes rather than relying on one retrieval channel.
- Deep Search direction suggests natural-language investigation over deterministic search primitives.

### Cursor
- Invests heavily in fast repo indexing and incremental freshness using Merkle-tree-based sync plus chunk embeddings.
- Key pattern: reuse teammate indexes and incremental sync to reduce time-to-first-query.
- Also uses rules as persistent prompt-layer context.
- Emerging weakness: memory features appear partly remote-hosted and privacy-mode tradeoffs are still evolving.

### Windsurf / Cascade
- Uses persistent memories plus explicit rules, codemaps, and remote indexing in enterprise modes.
- Strong pattern: conversation memory + rules + code maps as separate layers.
- Risk: single-agent persistent context can race when parallel work touches same files.

### OpenHands and adjacent harness work
- Strong on sandboxed execution and orchestration, lighter on public evidence of differentiated long-term memory design.
- Broader 2025–2026 harness literature converges on: context engineering, structured tools, planning artifacts, memory tiers, and evaluation.

## What appears state of the art

### 1. Hybrid retrieval, not single-mode retrieval
Best systems combine:
- exact lexical/FTS search
- symbol / graph search
- semantic vector retrieval
- recency / activity weighting
- task-scoped filtering

### 2. Memory tiers with different contracts
A recurring pattern across strong systems:
- hot memory: tiny, explicit, always-present instructions/state
- warm memory: task/repo-specific notes and plans
- cold memory: large searchable archive

### 3. Incremental freshness, not full rebuilds
Best-in-class systems avoid full reindex/re-embed whenever possible.
They track deltas, reuse prior indexes, and refresh only affected slices.

### 4. Separate “rules”, “memory”, and “structure”
The strongest products do not conflate:
- coding rules / preferences
- factual memory
- repository topology / dependency truth

### 5. Evaluation is part of the memory system
The mature pattern is to measure retrieval quality, memory usefulness, and stale-memory failures directly.

## Gaps in Djinn relative to this direction

### Gap A — no explicit hot/warm/cold memory contract
Djinn has many note types and retrieval modes, but not yet a clearly enforced runtime split between:
- always-loaded memory blocks
- task/repo-scoped working memory
- archival searchable knowledge

### Gap B — freshness is note-level, not fully event-driven across all knowledge artifacts
Djinn has reindexing and embeddings, but external survey suggests a stronger model:
- incremental refresh after edits
- stale detection by content hash / dependency impact
- proactive memory invalidation for changed code regions

### Gap C — repository structure and memory are connected, but not tightly enough
Djinn already has file-affinity ideas in ADR-043, but could go further by making graph events first-class retrieval signals.
Examples:
- changing a central symbol should decay or review related notes
- impacted notes should be re-ranked when nearby code changes

### Gap D — missing explicit “working spec” artifact for active investigations / implementation waves
Augment’s “living spec” pattern appears useful.
Djinn has ADRs, roadmap notes, tasks, and design notes, but not a unified lightweight artifact for an active problem decomposition that both humans and agents can keep fresh while work is in motion.

### Gap E — limited retrieval evaluation loop
Djinn has many good memory primitives, but lacks a visible benchmark/report loop for questions like:
- Did the right notes get retrieved?
- Which notes are never used?
- Which notes are repeatedly contradicted by code reality?
- Which note types drive successful task completion?

## Recommendations for Djinn

### Recommendation 1 — add explicit memory tiers
Define three first-class runtime categories:
- **Hot memory blocks**: tiny always-on project constitution (architecture constraints, repo conventions, current branch/canonical-view caveats, verification expectations)
- **Warm task memory**: per-task / per-epic active notes, planning summaries, current wave docs
- **Cold archive**: all other notes retrievable via FTS + embeddings + graph affinity

This should be a runtime contract, not just a conceptual note taxonomy.

### Recommendation 2 — create a “working spec” note type or convention
Add a lightweight durable artifact for active initiatives, separate from ADRs.
It should capture:
- current objective
- key files/symbols
- open questions
- agreed constraints
- recent findings
- next likely subproblems

This reduces repeated rediscovery across sessions.

### Recommendation 3 — tighten code→memory freshness coupling
On meaningful code change:
- compute impacted files/symbols from SCIP graph
- identify nearby notes by scope_paths, file affinity, and prior co-access
- mark candidate notes as “review-needed” or reduce freshness confidence
- optionally enqueue background refresh/summarization for those notes

This is more precise than broad reindex-only behavior.

### Recommendation 4 — make graph events a retrieval signal everywhere
Upgrade note retrieval ranking using:
- symbol/file proximity in code_graph
- centrality / impact of changed nodes
- task target file overlap
- recent edit paths

The goal is to move from “semantic memory search” to “semantic + structural + temporal” retrieval.

### Recommendation 5 — add memory quality telemetry
Track:
- note retrieval hit rate in successful tasks
- stale note detection rate
- contradiction / supersession frequency
- note usefulness by type
- notes frequently accessed together
- notes never retrieved over long windows

This would let Djinn improve memory based on observed utility rather than intuition.

### Recommendation 6 — support explicit repository maps / codemaps as durable memory assets
Cursor and Windsurf both lean on map-like artifacts.
Djinn already has repo maps in memory, but they should become more actionable:
- stable summaries for subsystems
- generated codemap notes for hotspots / central modules
- durable architectural overviews tied to graph centrality and boundary edges

### Recommendation 7 — maintain a tiny “project constitution” always in prompt
Instead of overloading brief/roadmap/ADR text, keep a compact always-on artifact containing:
- architecture invariants
- important forbidden couplings
- naming / module boundary rules
- memory freshness caveats
- known system assumptions

This mirrors Letta-style memory blocks but should remain file-backed and inspectable.

### Recommendation 8 — add stale-memory patrols with explicit outcomes
Periodic patrol should report one of:
- notes refreshed
- no drift found
- contradiction detected
- review-needed notes identified

This prevents silent or vague “memory maintenance”.

## Suggested implementation sequence

### Phase 1 — low-risk, high-leverage
- formalize hot/warm/cold memory contract
- introduce project constitution note
- introduce working-spec note/convention
- add memory telemetry counters

### Phase 2 — structural freshness
- code change → impacted note discovery
- freshness/confidence decay for potentially stale notes
- retrieval re-ranking with graph signals

### Phase 3 — codemap and evaluation loop
- generate durable subsystem codemaps/repo overviews
- add benchmark/report loop for retrieval quality
- expose note utility reports to architect/planner/chat

## Why now
Djinn already invested in SCIP, code_graph, and semantic memory.
The main remaining opportunity is not “add another search mode”, but to unify:
- structure truth from SCIP
- meaning from embeddings/FTS
- operational continuity from persistent working memory
- freshness from change-aware invalidation
- quality from eval/telemetry

That combination appears to be the actual frontier.

## Relations
- [[ADR-043 Repository Map — SCIP-Powered Structural Context for Agent Sessions]]
- [[ADR-050 Architect/Chat Code-Graph Consolidation, Canonical SCIP Indexing, and Graph Query Extensions]]
- [[ADR-053 Semantic Memory Search — Candle Embeddings with sqlite-vec]]
- [[Cognitive Memory Systems Research]]
