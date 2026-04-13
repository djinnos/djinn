---
title: ADR-042: DB-Only Knowledge Extraction, Consolidation, and Task Routing Fixes
type: 
tags: ["knowledge-extraction","consolidation","db-storage","task-routing","session-reflection"]
---


**Status:** Draft
**Date:** 2026-03-24
**Authors:** Fernando, Claude

Extends: [[decisions/adr-023-cognitive-memory-architecture-multi-signal-retrieval-and-associative-learning|ADR-023: Cognitive Memory Architecture]], [[ADR-038: Configurable Agent Roles]]

## Context

### The Knowledge Sprawl Problem

ADR-023 §7 defines session reflection: after a task completes, the system extracts cases, patterns, and pitfalls via a two-stage pipeline (structural extraction → LLM knowledge extraction). The pipeline is implemented and running. The problem is what it produces.

After ~63 architect patrol sessions and multiple worker sessions over two days, the knowledge base contains:

- ~45 cases, ~60 patterns, ~90 pitfalls
- Massive semantic duplication: dozens of near-identical notes about "blocked prerequisite seams" with slightly different wording
- All auto-extracted at confidence 0.5 — no quality gating prevents low-value writes

The dedup pipeline (ADR-023 §6) exists and fires on every `memory_write`, but BM25 similarity alone doesn't catch semantic duplicates with different surface wording. "Don't assume prerequisite seam exists" passes dedup against "verify prerequisite seam before proceeding" because the lexical overlap is below threshold.

### The Filesystem-as-Source-of-Truth Problem

The current architecture (from the original notes schema, `V20260303000002`) treats markdown files on disk as the source of truth:

```
memory_write → write .md file to .djinn/{type}/{slug}.md → insert DB row as search index
```

The `Note` model comment says: *"Source of truth is the markdown file on disk; this struct represents the SQLite index row."*

This made sense when notes were human-authored ADRs and research documents. It does not make sense for machine-generated cases/patterns/pitfalls because:

1. **Volume is unsustainable** — 200+ auto-generated files after two days. This will be thousands in a month.
2. **Git pollution** — these files appear as untracked in `git status`, bloating diffs and repo size if committed.
3. **No team sync path** — committing machine-generated noise to the repo is not how you share knowledge across a team. A DB-synced approach (future: Turso replicas, server API) is the right medium.
4. **Disk-first writes are unnecessary overhead** — the DB already has the full content, FTS index, confidence scores, associations, and L0/L1 summaries. The .md file is redundant for machine-generated notes.
5. **Reindex drift** — filesystem and DB can diverge since there's no filesystem watcher; only manual `memory_reindex` reconciles them.

### The Missing CONSOLIDATE Step

Comparing with Ruflo's self-learning loop (`RETRIEVE → JUDGE → DISTILL → CONSOLIDATE → ROUTE`), Djinn's current pipeline is `EXTRACT → WRITE`. The missing steps:

- **JUDGE**: Is this extraction novel? Does it add information beyond what's already known?
- **CONSOLIDATE**: Merge clusters of near-identical notes into single canonical notes with boosted confidence.

Without consolidation, repeated sessions on similar work (e.g., 20 sessions hitting the same blocked prerequisite) each dump 3-8 notes that say roughly the same thing. The dedup catches exact matches but not semantic equivalents.

### The Task Routing Gap

A related problem surfaced during investigation: task `1py1` ("Attach the existing ybre roadmap note permalink to epic memory_refs") has `issue_type: "task"` and routes to a Worker. But it requires `epic_update`, which Workers don't have — only Planner, Lead, and Architect roles do. The worker churns through 6 sessions and 2 reopens trying to accomplish something it literally cannot do.

Current dispatch rules route by `issue_type`:
- `"task"` → Worker
- `"decomposition"` → Planner
- `"spike"` | `"review"` → Architect

The `decomposition` type is too narrow — it only covers epic-to-task breakdown. Tasks that need epic management tools (attaching memory refs, updating metadata) but aren't decompositions have no route to a role that can handle them. The architect patrol creates these tasks with `issue_type: "task"` because that's the default, sending them to Workers that don't have `epic_update`.

### Reference: Ruflo Comparison (Actionable Items)

From the [[Ruflo Comparison Analysis]], the patterns most relevant to these problems:

| Ruflo Pattern | Djinn Gap | This ADR |
|---------------|-----------|----------|
| Self-learning CONSOLIDATE step | No periodic merge of duplicate knowledge | §2: Consolidation worker |
| Q-Learning Router | No learned dispatch; workers get tasks they can't complete | §4: Issue type routing |
| Per-tool metrics | Can't observe dedup skip rates or write volumes | §3: Extraction metrics |
| Error retryability | System retries permanently-failed tasks | §4: Failure memory |

## Decision

### 1. DB-Only Storage for Machine-Generated Notes

**Change the source of truth for auto-extracted note types** (`case`, `pattern`, `pitfall`) from filesystem to database. Human-authored types (`adr`, `research`, `design`, `requirement`, `reference`) remain filesystem-primary.

#### Schema change

Add a `storage` column to notes:

```sql
ALTER TABLE notes ADD COLUMN storage TEXT NOT NULL DEFAULT 'file';
-- 'file' = filesystem-primary (existing behavior)
-- 'db'   = database-only (no .md file on disk)
```

#### Write path change

In `NoteRepository::create()`:
- If `storage = 'file'`: current behavior (write .md, then insert DB row)
- If `storage = 'db'`: insert DB row only; `file_path` set to empty string

Auto-extracted notes from session reflection (`llm_extraction.rs`) always use `storage = 'db'`.

#### Read path

No change needed — `memory_read`, `memory_search`, `memory_list` already read from the DB. The only code that touches disk is `create`/`update`/`delete`/`reindex`, which will check the `storage` column.

#### Migration of existing files

A one-time migration:
1. For each note with `note_type` in (`case`, `pattern`, `pitfall`): set `storage = 'db'`
2. Delete the corresponding .md files from `.djinn/cases/`, `.djinn/patterns/`, `.djinn/pitfalls/`
3. Add these directories to `.gitignore` as a safety net

#### Human-authored notes remain on disk

ADRs, research docs, design docs, etc. stay filesystem-primary. These are curated, low-volume, and benefit from being committed to the repo (reviewable in PRs, readable without Djinn running). The `storage = 'file'` default preserves this.

#### Promotion path

A future `memory_promote` tool could convert a high-confidence DB-only note to a filesystem note (e.g., a pitfall that's been validated by 10 successful task references gets promoted to a committed .md file). Not in scope for this ADR.

### 2. Periodic Consolidation Worker

Add a background consolidation job that merges clusters of semantically similar notes.

#### Trigger

Runs after every N session reflections (default: 5) or on a timer (default: 1 hour), whichever comes first. Only processes `storage = 'db'` notes.

#### Algorithm

```
1. For each (project_id, note_type) combination:
   a. Fetch all notes with confidence < 0.8 (skip already-consolidated high-confidence notes)
   b. For each note, find candidates with BM25 similarity > threshold
   c. Build clusters using connected-component grouping
      (A similar to B, B similar to C → {A,B,C} is one cluster)
   d. For clusters of size >= 3:
      - Send all L0 abstracts to LLM
      - LLM produces: one canonical title, one consolidated content, merged tags
      - Create new note with confidence = min(0.8, 0.5 + 0.05 × cluster_size)
      - Delete or archive cluster members
      - Preserve provenance: consolidated note footer lists source session IDs
```

#### Cost control

- Only clusters of 3+ are consolidated (pairs might be legitimately distinct)
- LLM input is L0 abstracts only (~100 tokens each), not full content
- Estimated cost per consolidation run: ~2K tokens for a 5-note cluster

#### Result

Instead of 20 pitfalls saying "don't assume prerequisite seam exists" (each at 0.5 confidence), we get 1 pitfall at 0.75 confidence with richer content synthesized from all 20.

### 3. Extraction Quality Improvements

#### 3a. Pre-write novelty check

Before writing an extracted note, check if the note's *intent* (not just its words) is already covered. Add to `llm_extraction.rs`:

```
1. Generate L0 abstract for the candidate note (before writing)
2. Search existing notes of the same type with BM25
3. For top-3 matches: send candidate L0 + match L0 to LLM
4. LLM decides: "novel" or "already known"
5. On "already known": skip write, bump confidence on existing note instead
```

This is essentially the existing dedup but at the semantic level using L0 abstracts rather than raw BM25 on full content.

#### 3b. Fix access_count tracking

`memory_read` does not currently increment `access_count` on the note. This means the temporal priority signal (ACT-R activation from ADR-023 §1) is effectively dead. Fix:

```rust
// In memory_read handler, after fetching the note:
repo.increment_access_count(note_id).await?;
```

#### 3c. Extraction metrics

Track per-session:
- Notes extracted (by type)
- Notes skipped by dedup
- Notes skipped by novelty check (§3a)
- Notes written

Expose via `agent_metrics` tool so the architect patrol can observe extraction health and flag anomalies (e.g., "last 10 sessions extracted 0 novel notes" = the system is learning nothing new).

### 4. Task Routing Fixes

#### 4a. Rename `decomposition` → `planning`

The existing `decomposition` issue type is too narrow — it only describes one thing the Planner does (breaking epics into tasks). Rename it to `planning` to cover the full scope of Planner work:

- Epic decomposition into tasks (current `decomposition` behavior)
- Attaching memory refs to epics
- Updating epic descriptions or acceptance criteria
- Reconciling metadata between epics and the knowledge base
- Re-prioritizing and reorganizing work

Dispatch rule change: `planning` → Planner (replaces `decomposition` → Planner).

Migration: update existing tasks with `issue_type = 'decomposition'` to `issue_type = 'planning'`.

#### 4b. Architect patrol creates tasks with correct issue type

When the architect creates tasks that involve epic metadata operations, it should set `issue_type: "planning"` instead of the default `"task"`. Update the architect prompt to include guidance:

```
When creating tasks that require epic updates (memory_refs, description, acceptance criteria),
set issue_type to "planning" so they route to the Planner which has epic management tools.
Workers cannot modify epics.
```

#### 4c. Dispatch failure memory

Track task dispatch outcomes. If a task has been reopened N times (default: 3) without progress:

1. The architect patrol flags it during board health review
2. If the task's required tools don't match the dispatched role's tool set, the architect should re-create it with the correct `issue_type` or escalate

This is a lightweight version of Ruflo's Q-Learning Router — not learned routing, but observable failure signals that the architect (already running every 5 minutes) can act on.

## Consequences

**Positive:**
- Knowledge base stops growing unboundedly with near-identical notes
- Git status is clean — no more 200+ untracked files in `.djinn/`
- Consolidated notes have higher confidence and richer content than any single extraction
- DB-only storage enables future team sync via Turso replicas or server API without git commits
- Tasks route to roles that can actually complete them
- Extraction metrics make knowledge quality observable
- Human-authored notes (ADRs, research) remain on disk — no workflow change for those
- access_count fix enables temporal priority signal that was designed but broken

**Negative:**
- DB-only notes are not inspectable without Djinn running (mitigated: `memory_read`/`memory_search` via MCP, or direct SQLite queries)
- Consolidation worker adds background job complexity
- LLM cost for novelty checks (~500 tokens per extraction) adds to the per-session overhead (from ~6K to ~8K tokens)
- Renaming `decomposition` → `planning` requires migrating existing tasks and updating any code/prompts that reference the old name

**Migration risk:**
- Existing .md files for cases/patterns/pitfalls can be safely deleted since the DB already has the content
- The `file_path` column on migrated notes will be empty; code that reads from disk must check `storage` column first
- Reindex (`memory_reindex`) must skip `storage = 'db'` notes to avoid marking them as "deleted from disk"

## Phasing

**Phase 42a: DB-only storage** (immediate)
- Add `storage` column, update write/read/delete/reindex paths
- Session reflection writes with `storage = 'db'`
- Migrate existing case/pattern/pitfall notes
- Delete .md files, add to .gitignore

**Phase 42b: Consolidation worker** (after 42a)
- Background job for cluster detection and merge
- Provenance tracking on consolidated notes
- Metrics for consolidation runs

**Phase 42c: Extraction quality** (parallel with 42b)
- Pre-write novelty check using L0 abstracts
- Fix access_count increment in memory_read
- Extraction metrics per session

**Phase 42d: Task routing** (independent, can start immediately)
- Rename `decomposition` → `planning` in dispatch rules, prompts, and existing tasks
- Update architect prompt for correct issue_type on epic-related tasks
- Dispatch failure observation in architect patrol

## Relations

- [[ADR-023: Cognitive Memory Architecture]] — extended (DB-only storage for machine-generated notes, consolidation as missing CONSOLIDATE step, access_count fix)
- [[ADR-038: Configurable Agent Roles]] — complementary (extraction metrics feed into agent effectiveness monitoring, routing fixes improve dispatch)
- [[decisions/adr-034-agent-role-hierarchy-architect-patrol-scrum-master-rules-and-task-types|ADR-034]] — extended (`decomposition` renamed to `planning`, architect prompt update)
- [[Ruflo Comparison Analysis]] — inspiration for consolidation loop, dispatch failure memory, tool metrics