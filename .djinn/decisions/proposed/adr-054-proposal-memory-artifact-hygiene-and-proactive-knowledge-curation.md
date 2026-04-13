---
title: ADR-054 Proposal: Memory Artifact Hygiene and Proactive Knowledge Curation
type: adr
tags: ["adr","memory","knowledge-hygiene","extraction","taxonomy","curation"]
---

# ADR-054 Proposal: Memory Artifact Hygiene and Proactive Knowledge Curation

## Status

Proposed

Date: 2026-04-13

Related: [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]], [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]], [[ADR-042: DB-Only Knowledge Extraction, Consolidation, and Task-Routing Fixes]], [[Repository Understanding and Memory Freshness Upgrade Path]]

## Context

Djinn already has a strong memory retrieval substrate:

- filesystem-backed notes as the durable source of truth
- FTS + multi-signal ranking from `NoteRepository::search(...)`
- graph proximity, temporal scoring, task affinity, and confidence weighting
- checksum-based incremental reindexing from disk
- session-time knowledge extraction in `llm_extraction.rs`
- semantic retrieval being added by [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]]

The current bottleneck is no longer primarily retrieval. It is the **shape and quality of stored knowledge artifacts**.

The extraction implementation still explicitly funnels learned knowledge into only three mergeable durable buckets:

- `cases`
- `patterns`
- `pitfalls`

`llm_extraction.rs` documents this directly:
- cases = problem + solution pairs from successful task outcomes
- patterns = reusable processes or methods discovered during the session
- pitfalls = errors encountered and how they were resolved

In practice, those buckets are absorbing too many different abstraction levels:

- one-off implementation outcomes
- task-local repair notes
- stable reusable engineering rules
- transient migration tactics
- historical precedents
- cleanup observations
- ongoing maintenance context

This makes the memory system feel noisy even when retrieval is working correctly.

### Evidence observed in current project memory

1. **Folder volume and hygiene pressure are high**
   - total notes: 877
   - orphan notes: 798
   - broken links: 136

2. **Recent extracted notes show type blur**
   Sampled notes from the current semantic-search work include:
   - `cases/blend-semantic-retrieval-into-existing-note-search-without-changing-the-mcp-interface`
   - `patterns/add-new-retrieval-signals-behind-a-stable-search-interface`
   - `pitfalls/ranking-regression-issues-when-mixing-vector-results-with-existing-fts-rrf-search`
   - `patterns/attach-semantic-side-effects-directly-to-note-lifecycle-operations`
   - `pitfalls/embedding-sync-can-be-missed-when-note-mutations-enter-through-multiple-code-paths`

   These are all useful, but several are semantically near each other and differ more by phrasing than by true note kind.

3. **The catalog shows large families of near-duplicate or near-sibling notes**
   Examples include repeated variants such as:
   - "Blend semantic retrieval into existing note search without changing the MCP interface"
   - "Blend semantic vector search into existing memory search ranking"
   - "Added vector-aware ranking to the existing note search pipeline"
   - multiple concurrency/schema safety notes with highly similar semantic payloads

4. **Many notes are clearly session-extracted but too lightly normalized**
   Their content often ends as one compact paragraph plus a provenance footer. This is cheap to produce and easy to index, but often too underspecified to be strong durable memory.

The result is a knowledge base that is rich but increasingly messy: retrieval can find relevant notes, yet the returned artifacts are often too generic, too local, too duplicative, or too weakly typed to be maximally useful to humans and agents.

## Problem statement

Djinn currently optimizes for **capturing** knowledge, but not enough for **curating, consolidating, and upgrading** it.

Without stronger artifact hygiene, ADR-053 embeddings will make it easier to find semantically related notes, but may also surface more low-grade near-duplicates and type-blurred content. Better search over weak artifacts does not produce better understanding.

The system needs to become proactive in two ways:

1. **Proactive codebase learning**
   - build and refresh durable subsystem understanding from code structure and code change
   - do not rely only on what happened to be mentioned in a task session

2. **Proactive memory cleanup and promotion**
   - detect weak, duplicate, stale, or overly local extracted notes
   - consolidate them into stronger durable artifacts
   - separate transient working memory from long-lived reusable memory

## Decision

Adopt a **Memory Artifact Hygiene** layer on top of ADR-023 and ADR-053.

This proposal does **not** primarily change search. It changes how Djinn creates, evaluates, consolidates, and refreshes note content so the search layer has better material to retrieve.

### 1. Split durable knowledge from transient/session-local knowledge

Keep `pattern`, `pitfall`, and `case`, but narrow their meaning.

#### Pattern
Use only for a reusable recommendation that should plausibly guide multiple future tasks.

A valid pattern must answer:
- when to use it
- why it helps
- boundaries / tradeoffs
- what kind of situation it generalizes across

#### Pitfall
Use only for a recurring failure mode with a recognizable trigger and a prevention heuristic.

A valid pitfall must answer:
- how to recognize the smell early
- what goes wrong
- how to prevent it
- how to recover if triggered

#### Case
Use only for a worked precedent that is useful mainly as an example.

A valid case must answer:
- what situation occurred
- what constraint mattered
- what was done
- why it worked or failed
- what lesson transfers from it

#### New convention: Working memory is not durable memory

A substantial share of current extracted notes are better understood as **working context** than as durable memory. Introduce a new artifact for mutable, active understanding:

- `design` note subtype or explicit convention: **Working Spec**

A Working Spec captures:
- active objective
- relevant files/symbols/scope paths
- discovered constraints
- current hypotheses
- open questions
- likely next decomposition

This absorbs material that should not be immortalized immediately as a pattern/pitfall/case.

### 2. Add note quality thresholds before durable extraction is committed

Session extraction should not write every plausible observation as a first-class durable note.

Before creating a `pattern`, `pitfall`, or `case`, require the extractor or a post-processor to evaluate:

- **specificity** — is the note concrete enough to be useful?
- **generality** — is it broader than one task-local patch note?
- **novelty** — is it materially different from existing nearby notes?
- **durability** — is this likely to remain useful after the current branch/task wave?
- **type fit** — does it really fit case/pattern/pitfall semantics?

Outcomes:
- `durable_write` — create/update the durable note
- `merge_into_existing` — strengthen an existing note instead of creating a sibling
- `downgrade_to_working_spec` — keep as active context, not durable memory
- `discard` — provenance remains in session history, but no memory note is created

### 3. Add stronger templates for durable note types

A major source of generic notes is understructured output. Durable note types should use strict templates.

#### Pattern template
- Context
- Problem shape
- Recommended approach
- Why it works
- Tradeoffs / limits
- When to use
- When not to use
- Related cases/pitfalls

#### Pitfall template
- Trigger / smell
- Failure mode
- Observable symptoms
- Prevention
- Recovery
- Related pattern/case

#### Case template
- Situation
- Constraint
- Approach taken
- Result
- Why it worked / failed
- Reusable lesson
- Related pattern/pitfall

Notes missing these sections should be considered low-quality and candidates for later consolidation.

### 4. Introduce proactive consolidation passes

Memory hygiene should not depend only on accidental human cleanup.

Add background or patrol-style curation passes that:

- cluster near-duplicate notes by embedding similarity + folder/type + scope overlap
- detect title families that differ only by phrasing
- identify underspecified one-paragraph extracted notes
- identify notes with no inbound links, no reuse, and low confidence
- propose merges or promotions

Typical transformations:
- merge 3 weak sibling notes into 1 strong durable note
- convert a weak pattern into a case
- fold repeated incidents into one stronger pitfall
- promote repeated working-spec findings into an ADR, pattern, or pitfall
- archive or down-rank dead residue

### 5. Learn proactively from the codebase, not only from task sessions

The current extraction flow is heavily session-derived. That misses structural truths the codebase can reveal directly.

Add a proactive learning loop that periodically uses:
- `code_graph`
- repository maps
- scope paths on notes
- recent code changes
- symbol hotspots
- dependency boundaries

to generate or refresh higher-quality memory such as:
- subsystem overviews
- boundary notes
- hotspot notes
- module-role summaries
- "what changed in this subsystem" refresh notes

This learning loop should preferentially update durable overview/design artifacts rather than spraying more case/pattern/pitfall notes.

### 6. Attach freshness and review semantics to durable notes

A note can be semantically retrievable and still be stale.

Add review metadata for durable knowledge:
- freshness status: `fresh`, `review_needed`, `stale`
- review basis: `code_changed`, `duplicate_cluster`, `contradiction`, `manual_review`, `low_reuse`
- last reviewed at
- reviewed against scope paths / changed files

When code changes intersect a note's `scope_paths` or nearby graph region:
- reduce freshness
- surface the note to a cleanup/review queue
- prefer fresher siblings in retrieval/ranking

### 7. Measure note usefulness, not just note existence

Introduce note-quality telemetry so cleanup is evidence-driven.

Candidate signals:
- retrieval count
- read-after-retrieval count
- citation count in tasks/ADRs/designs
- co-access in successful tasks
- stale-hit incidents
- merge frequency
- duplicate-cluster membership
- time since last useful access

Use these signals to distinguish:
- canonical durable notes
- promising but weak notes that need consolidation
- dead residue that should be archived or suppressed

## Why now

This issue is surfacing now for four reasons:

1. **Scale** — the KB is now large enough that weak note shape is visible in normal use.
2. **Implementation maturity** — retrieval and indexing are already good enough that content quality is now the limiting factor.
3. **ADR-053** — semantic search will improve recall, which increases the importance of note precision and consolidation.
4. **Current extraction behavior** — `llm_extraction.rs` is actively writing into broad durable buckets, so taxonomy debt compounds as work continues.

## Alternatives considered

### A. Do nothing and rely on embeddings to smooth over note messiness
Rejected. Better retrieval helps recall, but it does not fix weak, duplicative, or type-blurred artifacts.

### B. Replace pattern/pitfall/case with many new note types immediately
Rejected for now. Exploding the taxonomy too early would add churn before we have quality gates and consolidation behavior.

### C. Keep the taxonomy but improve prompts only
Insufficient. Prompt changes help, but the system also needs post-write evaluation, merge behavior, and cleanup passes.

### D. Make all learning live only in session summaries or chat context
Rejected. Djinn needs durable, inspectable, git-tracked memory.

## Consequences

### Positive
- higher-quality durable notes
- less semantic duplication across pattern/pitfall/case folders
- better human trust in retrieved memory
- stronger proactive codebase understanding
- a cleaner substrate for ADR-053 embeddings
- reduced orphan and broken-link pressure over time through explicit consolidation and promotion

### Negative
- more complexity in extraction and memory maintenance
- some extracted observations will no longer become immediate durable notes
- requires migration/cleanup work for the existing corpus
- quality scoring heuristics may initially misclassify some useful notes

## Migration / rollout

### Phase 1 — tighten write semantics
- add templates and quality gates for case/pattern/pitfall extraction
- add a Working Spec convention using existing `design` notes
- stop writing weak task-local observations as durable notes by default

### Phase 2 — add hygiene patrols
- detect duplicate clusters
- identify underspecified extracted notes
- propose merges/promotions
- add freshness/review metadata

### Phase 3 — proactive codebase learning
- periodic subsystem overview generation from code graph + repo maps
- note refresh when scoped code changes materially
- promote repeated structural findings into durable summaries/ADRs

### Phase 4 — corpus cleanup
- consolidate existing semantic sibling families
- repair or archive dead residue
- reduce orphan-heavy low-value note populations

## Suggested follow-on work

If accepted, this proposal should likely spawn:

1. an epic for memory artifact hygiene and proactive curation
2. a planning task to define strict extraction templates and quality gates
3. a planning task to design the working-spec convention and cleanup patrol flow
4. a planning task to audit the existing `pattern` / `pitfall` / `case` corpus for merge candidates

## Relations
- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]
- [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]]
- [[Repository Understanding and Memory Freshness Upgrade Path]]
