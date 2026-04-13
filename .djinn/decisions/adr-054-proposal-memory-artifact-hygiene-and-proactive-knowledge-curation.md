---
title: "ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy"
type: adr
tags: ["adr","memory","extraction","quality-gates","taxonomy","templates"]
---

# ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy

## Status

Proposed

Date: 2026-04-13

Related: [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]], [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]], [[ADR-042: DB-Only Knowledge Extraction, Consolidation, and Task-Routing Fixes]], [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]], [[ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene]]

## Context

Djinn's knowledge base has grown to ~877 notes with 798 orphans and 136 broken links. The retrieval substrate is solid (FTS, multi-signal ranking, graph proximity, confidence weighting, embeddings via ADR-053), but the **quality of stored artifacts** is now the bottleneck.

The extraction implementation in `llm_extraction.rs` funnels all learned knowledge into three durable buckets:

- `cases` — problem + solution pairs from successful task outcomes
- `patterns` — reusable processes or methods discovered during the session
- `pitfalls` — errors encountered and how they were resolved

In practice, these buckets absorb too many abstraction levels: one-off implementation outcomes, task-local repair notes, stable reusable rules, transient migration tactics, historical precedents, and ongoing maintenance context. The result is a knowledge base where retrieval works but returned artifacts are too generic, too local, too duplicative, or too weakly typed.

### Evidence

1. **Type blur in recent extractions** — sampled notes from semantic-search work include `cases/blend-semantic-retrieval-into-existing-note-search-without-changing-the-mcp-interface` alongside `patterns/add-new-retrieval-signals-behind-a-stable-search-interface` — these differ by phrasing more than by true note kind.

2. **Large near-duplicate families** — repeated variants like "Blend semantic retrieval into existing note search...", "Blend semantic vector search into existing memory search ranking", "Added vector-aware ranking to the existing note search pipeline".

3. **Underspecified extracted notes** — many notes are one compact paragraph plus a provenance footer. Cheap to produce and easy to index, but too underspecified to be strong durable memory.

### What already exists

The system already tracks signals that this ADR builds on:

- `access_count` and `last_accessed` on every note (used in temporal scoring)
- Bayesian confidence with signals: `USER_CONFIRM` (0.95), `CO_ACCESS_HIGH` (0.65), `STALE_CITATION` (0.3), `CONTRADICTION` (0.1)
- Novelty detection in `llm_extraction.rs` via FTS5 BM25 dedup (3 candidates)
- LLM-extracted notes start at confidence 0.5 (vs 1.0 for human-written)
- `NoteConsolidationRepository` with `likely_duplicate_clusters()` and `synthesize_cluster()`
- Orphan flagging via `flag_orphan_notes()` (zero inbound links + not accessed in 30 days + access_count=0)

The gap is that these signals exist but are not wired into a quality gate that prevents weak notes from becoming durable in the first place.

## Problem statement

Djinn optimizes for **capturing** knowledge but not for **curating it at write time**. Session extraction writes every plausible observation as a first-class durable note. Without quality gates, ADR-053 embeddings will surface more low-grade near-duplicates. Better search over weak artifacts does not produce better understanding.

This ADR focuses narrowly on **what happens at extraction time** — tighter taxonomy, quality thresholds, and structured templates. Broader concerns (storage-layer branching, Planner-driven hygiene patrols, codebase learning) are addressed by ADR-055 and ADR-056.

## Decision

### 1. Tighten note type semantics

Keep `pattern`, `pitfall`, and `case`, but enforce narrower definitions.

#### Pattern

A reusable recommendation that should plausibly guide multiple future tasks.

Must answer:
- When to use it
- Why it helps
- Boundaries / tradeoffs
- What kind of situation it generalizes across

#### Pitfall

A recurring failure mode with a recognizable trigger and a prevention heuristic.

Must answer:
- How to recognize the smell early
- What goes wrong
- How to prevent it
- How to recover if triggered

#### Case

A worked precedent useful mainly as a concrete example.

Must answer:
- What situation occurred
- What constraint mattered
- What was done
- Why it worked or failed
- What lesson transfers from it

### 2. Introduce the Working Spec convention

A substantial share of current extracted notes are better understood as **working context** than as durable memory. Introduce:

**Working Spec** (`design` note subtype) — mutable, session-scoped understanding that captures:
- Active objective
- Relevant files/symbols/scope paths
- Discovered constraints
- Current hypotheses
- Open questions
- Likely next decomposition

Working Specs are explicitly **not durable**. They live on the task's knowledge branch (see ADR-055) and are discarded or promoted at task completion, never automatically persisted as permanent notes.

### 3. Add quality gates before durable extraction

Before `llm_extraction.rs` creates a `pattern`, `pitfall`, or `case`, evaluate:

- **Specificity** — is the note concrete enough to be actionable?
- **Generality** — is it broader than one task-local observation?
- **Novelty** — is it materially different from existing notes? (Use embedding nearest-neighbor from ADR-053, not just 3-candidate FTS5 BM25)
- **Durability** — will this remain useful after the current task/branch?
- **Type fit** — does it actually match case/pattern/pitfall semantics?

Outcomes:
- `durable_write` — create the durable note (or update existing)
- `merge_into_existing` — strengthen an existing note's content and boost its confidence instead of creating a sibling
- `downgrade_to_working_spec` — keep as task-scoped working context
- `discard` — provenance remains in session history, no note created

### 4. Enforce structured templates for durable notes

A major source of generic notes is understructured LLM output. Require durable note types to follow strict templates.

#### Pattern template
```
## Context
## Problem shape
## Recommended approach
## Why it works
## Tradeoffs / limits
## When to use
## When not to use
## Related
```

#### Pitfall template
```
## Trigger / smell
## Failure mode
## Observable symptoms
## Prevention
## Recovery
## Related
```

#### Case template
```
## Situation
## Constraint
## Approach taken
## Result
## Why it worked / failed
## Reusable lesson
## Related
```

Notes missing required sections are rejected by the quality gate and downgraded to working spec.

### 5. Wire `touch_accessed` into MCP read paths

The `access_count` signal exists but `get()`, `search()`, and `resolve()` don't auto-increment it. Wire `touch_accessed()` into the `memory_read` and `memory_search` MCP handlers so access data is real and usable for freshness/usefulness scoring.

### 6. Use embedding similarity for novelty detection

Replace the current 3-candidate FTS5 BM25 dedup check in `llm_extraction.rs` with:

1. Compute embedding of candidate note content (ADR-053 infrastructure)
2. Query nearest neighbors (top 5) by cosine similarity
3. If any neighbor exceeds similarity threshold (e.g. 0.85), treat as semantic duplicate
4. Boost existing note confidence via `DUPLICATE_CONFIDENCE_SIGNAL` instead of creating a new note

This leverages ADR-053 embeddings to catch duplicates that FTS5 keyword matching misses.

## Alternatives considered

### A. Do nothing and rely on embeddings to smooth over note messiness
Rejected. Better retrieval helps recall but does not fix weak, duplicative, or type-blurred artifacts.

### B. Replace pattern/pitfall/case with many new note types immediately
Rejected. Exploding the taxonomy before quality gates exist would add churn without improving quality.

### C. Keep the taxonomy but improve prompts only
Insufficient. Prompt changes help, but extraction also needs post-write evaluation and merge-into-existing behavior.

### D. Application-layer quality gates without storage-layer branching
Viable as a standalone improvement. This ADR is designed to work both with current SQLite and with the Dolt migration proposed in ADR-055. Quality gates at extraction time reduce noise regardless of storage layer.

## Consequences

### Positive
- Higher-quality durable notes with enforced structure
- Fewer near-duplicate note families
- Session-local knowledge stays scoped instead of polluting the canonical KB
- Embedding-based novelty detection catches duplicates that keyword matching misses
- Access tracking becomes real and usable for downstream scoring

### Negative
- More complexity in extraction pipeline
- Some useful observations may be discarded or downgraded prematurely
- Template enforcement may feel rigid for edge-case knowledge
- Quality gate LLM calls add latency to post-session extraction

## Migration / rollout

### Phase 1 — Wire existing signals
- Wire `touch_accessed` into MCP read/search paths
- Switch novelty detection from FTS5 to embedding similarity

### Phase 2 — Tighten extraction
- Add quality gate evaluation to `llm_extraction.rs`
- Implement template enforcement for durable note types
- Add Working Spec convention to `design` notes

### Phase 3 — Corpus cleanup
- Audit existing `pattern` / `pitfall` / `case` notes against new templates
- Consolidate notes that fail quality thresholds
- Demote underspecified notes to working specs or archive

## Relations

- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]
- [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]]
- [[ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching]]
- [[ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene]]
