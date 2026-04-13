---
title: ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching
type: adr
tags: ["adr","dolt","database","branching","migration","qdrant","knowledge-management"]
---



# ADR-055 Proposal: Dolt Migration and Per-Task Knowledge Branching

## Status

Proposed

Date: 2026-04-13

Related: [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]], [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]], [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]], [[ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene]]

## Context

Djinn's knowledge base suffers from a fundamental architectural problem: session-extracted knowledge is written directly into the canonical knowledge base. Every task's observations, however speculative or task-local, immediately pollute the shared namespace. The result is 877 notes with 798 orphans — a knowledge base that is rich but increasingly noisy.

[[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]] addresses the write-quality side (better gates before extraction commits). But even perfect quality gates cannot solve the isolation problem: parallel agents writing to the same knowledge store interfere with each other, and there is no way to speculatively capture knowledge during a task and then decide what to promote afterward.

### The branching insight

Git solved this exact problem for code: work on a branch, review via PR, merge what's good, discard what's not. Dolt (dolthub/dolt) applies Git semantics to SQL databases — branches, commits, merges, diffs, history, all exposed as SQL stored procedures over a MySQL wire protocol.

### Prior art: Gas Town

The reference project at `/home/fernando/git/references/gastown` demonstrates a production Dolt integration:

- Single Dolt SQL server per deployment (port 3307), MySQL protocol
- All agents write to `main` via transaction discipline (`BEGIN` → `DOLT_COMMIT` → `COMMIT`)
- Integration branches for epics with auto-detection and safe landing guards
- 6-stage data lifecycle: CREATE → LIVE → CLOSE → DECAY → COMPACT → FLATTEN
- Automated compaction via Compactor Dog (24h interval, 2000-commit threshold)
- Wisps (ephemeral data) separated from durable data via `dolt_ignore`
- Serialized merge queue via merge slot bead
- Full history queryable via `dolt_history_*`, `AS OF`, `dolt_diff`

### Current SQLite coupling

The existing database layer uses `sqlx` 0.8 (async) + `rusqlite` 0.32 (migrations) with tight coupling to three SQLite-specific features:

1. **FTS5** — `notes_fts` virtual table with BM25 scoring, trigger-based sync
2. **sqlite-vec** — `note_embeddings_vec` vec0 virtual table for cosine similarity search
3. **SQLite pragmas** — WAL mode, busy timeout, cache size, foreign keys

### Why Dolt over application-layer branching

Adding a `branch` column to the notes table was considered and rejected. It provides shallow isolation (filtered queries) but none of the real benefits:

- No true merge semantics with conflict detection
- No history or blame on knowledge changes
- No rollback capability
- No cross-branch reads without checkout
- No diff between what a task learned vs what was already known

Dolt provides all of these as native SQL operations.

## Problem statement

Djinn needs **storage-layer isolation** for knowledge produced during parallel task execution, with a **promotion flow** that evaluates and merges worthy knowledge into the canonical store. The current single-namespace SQLite database cannot provide this.

## Decision

Migrate from SQLite to **Dolt** as the single database for all Djinn state (tasks, sessions, agents, notes, epics — everything). Replace **sqlite-vec** with **Qdrant** as a dedicated vector search sidecar. Implement **per-task knowledge branches** with a quality-gated merge flow.

### 1. Storage topology

```
dolt sql-server (port 3307)              qdrant (port 6333)
├── notes                                ├── notes collection
├── note_links                           │   ├── vectors (768d, cosine)
├── note_associations                    │   └── payloads (note_id, branch,
├── note_embeddings (metadata only)      │       scope_paths, tags)
├── tasks                                └── (future collections)
├── task_blockers
├── task_activity_log
├── sessions
├── session_messages
├── agents
├── epics
├── credentials
├── settings
├── consolidated_note_provenance
├── consolidation_run_metrics
├── verification_cache
├── verification_results
├── repo_map_cache
├── repo_graph_cache
└── model_health
```

**Dolt** handles all relational data, branching, history, and merge. Replaces SQLite entirely. Single DB matching Gas Town's approach — tasks/sessions don't need branching but don't suffer from it either, and operational simplicity of one server outweighs the marginal latency cost.

**Qdrant** handles vector search only. Replaces sqlite-vec. Purpose-built for vector search with a native async Rust client (`qdrant-client`), payload filtering, batch operations, and independent scaling. Embeddings computed by Candle (ADR-053) are stored in Qdrant with metadata payloads including `branch` for branch-aware search.

### 2. Full-text search replacement

Replace FTS5 with MySQL `FULLTEXT` indexes:

```sql
ALTER TABLE notes ADD FULLTEXT INDEX notes_ft (title, content, tags);

-- Query:
SELECT id, MATCH(title, content, tags) AGAINST(? IN BOOLEAN MODE) AS score
FROM notes
WHERE MATCH(title, content, tags) AGAINST(? IN BOOLEAN MODE)
ORDER BY score DESC
LIMIT ?;
```

MySQL FULLTEXT lacks FTS5's BM25 tuning knobs (`bm25(notes_fts, 3.0, 1.0, 2.0)`), but the existing RRF fusion already blends multiple signals (FTS, temporal, graph proximity, task affinity, embedding similarity). FTS is one candidate generator among several — it does not need to be perfect, it needs to be reasonable.

If FTS quality proves insufficient, Qdrant supports sparse vectors which can provide BM25-equivalent retrieval, consolidating both dense and sparse search in one service.

### 3. Per-task knowledge branches

```
main (canonical knowledge)
├── task_{task_id_1} (worker's session-extracted knowledge)
├── task_{task_id_2} (another worker's extraction)
└── task_{task_id_3} (architect spike findings)
```

**On task dispatch** (in coordinator dispatch pipeline):
```sql
CALL DOLT_BRANCH('task_{task_id}', 'main');
```

**During session** — extraction writes to the task branch:
```sql
-- Session's connection checks out task branch
CALL DOLT_CHECKOUT('task_{task_id}');

-- All note writes go to this branch
INSERT INTO notes (...) VALUES (...);
CALL DOLT_COMMIT('-Am', 'extract: {note_title}');
```

The task branch inherits all of main's data via Prolly Tree structural sharing (zero copy cost). Reads see both the task's new knowledge and all existing canonical knowledge.

**Qdrant sync**: Embeddings for task-branch notes go into Qdrant with a `branch` payload field. Branch-aware search filters on `branch IN [task_{id}, main]`.

### 4. Knowledge merge on task completion

When a task completes and its code PR merges, the knowledge PR flow runs:

**Step 1 — Diff**:
```sql
SELECT * FROM dolt_diff('main', 'task_{task_id}', 'notes');
```

**Step 2 — Quality gate** (from [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]]):
Each note added on the task branch is evaluated for specificity, generality, novelty, durability, and type fit. Outcomes: `promote`, `merge_into_existing`, `discard`.

**Step 3 — Selective merge**:
```sql
-- Option A: merge entire branch (if all notes passed)
CALL DOLT_CHECKOUT('main');
CALL DOLT_MERGE('task_{task_id}');
CALL DOLT_COMMIT('-Am', 'merge knowledge from task {task_id}: {summary}');

-- Option B: cherry-pick approved commits only
CALL DOLT_CHERRY_PICK('{commit_hash}');
```

**Step 4 — Qdrant sync**: Update `branch` payload from `task_{id}` to `main` for promoted notes. Delete vectors for discarded notes.

**Step 5 — Cleanup**:
```sql
CALL DOLT_BRANCH('-d', 'task_{task_id}');
```

### 5. Task abandonment

If a task is abandoned or closed without merge:
```sql
CALL DOLT_BRANCH('-d', 'task_{task_id}');
```
Zero cost — branch pointer deleted, data GC'd by Dolt's automatic garbage collection (triggers at 50MB journal, available since Dolt 1.75.0). Qdrant vectors with `branch = task_{id}` are deleted.

This is a key advantage: speculative knowledge that doesn't pan out is simply discarded without ever touching the canonical store.

### 6. Data lifecycle and compaction

Following Gas Town's proven pattern, implement a lifecycle for commit history:

| Stage | Mechanism | Interval |
|-------|-----------|----------|
| **LIVE** | Active commits from task branches and merges | Continuous |
| **COMPACT** | `DOLT_RESET('--soft')` + `DOLT_COMMIT` to squash old history | Daily (threshold: 2000 commits) |
| **FLATTEN** | Squash entire history to 1 commit at quiet hours | Weekly at 03:00 UTC (threshold: 5000 commits) |
| **GC** | Automatic since Dolt 1.75.0 (50MB journal trigger) | Automatic |

Compaction runs in the coordinator's hourly tick. Flatten runs as a scheduled maintenance task. Both follow Gas Town's safety pattern: record row counts before, verify after, abort on mismatch.

### 7. Connection architecture

```rust
// MySQL protocol via sqlx
let pool = MySqlPoolOptions::new()
    .max_connections(8)
    .after_connect(|conn, _meta| {
        Box::pin(async move {
            // Set session defaults
            sqlx::query("SET @@autocommit = 0")
                .execute(&mut *conn).await?;
            Ok(())
        })
    })
    .connect("mysql://root@127.0.0.1:3307/djinn")
    .await?;
```

**Per-session branch checkout**: Each agent session gets a dedicated connection that checks out the task's branch. The coordinator and MCP tools use connections on `main`.

**Dolt server management**: Following Gas Town's `DoltServerManager` pattern — health checks every 30 seconds, exponential backoff restart, fail-fast on startup if server unavailable.

### 8. History-powered features (enabled by migration)

Once on Dolt, these become possible with zero additional infrastructure:

- **Knowledge blame**: `SELECT * FROM dolt_history_notes WHERE id = ?` — see when a note was created, from which task, how it evolved
- **Time-travel context**: `SELECT * FROM notes AS OF 'HEAD~10'` — what did we know about this module last week?
- **Knowledge diff in patrol**: `SELECT * FROM dolt_diff('HEAD~50', 'HEAD', 'notes')` — what changed in the KB since last patrol?
- **Safe rollback**: `CALL DOLT_REVERT('{commit_hash}')` — undo a bad knowledge merge instantly
- **Audit trail**: `SELECT message, date FROM dolt_log WHERE message LIKE 'merge knowledge%'` — full history of knowledge merges

## Alternatives considered

### A. Application-layer branching (branch column in SQLite)
Rejected. Provides filtered isolation but no real merge, no history, no conflict detection, no rollback, no cross-branch reads. A half-measure that adds complexity without the full benefit.

### B. Keep SQLite, add Qdrant only
Viable for vector search upgrade, but doesn't solve the isolation problem. Parallel agents still write to the same knowledge store. No history or blame capabilities.

### C. PostgreSQL instead of Dolt
PostgreSQL is a stronger general-purpose DB but lacks native branching, merge, and history semantics. Would require building all version control at the application layer — essentially reimplementing what Dolt provides natively.

### D. Split DB (Dolt for knowledge, SQLite for operational)
Considered. Lower migration risk but doubles operational surface (two connection pools, two migration systems, two backup strategies). Gas Town runs everything in Dolt successfully. The 1-5ms IPC overhead is negligible compared to LLM call latencies that dominate actual wall time.

### E. Wait for DoltLite
DoltLite (announced 2026-03-25) is a SQLite fork with Prolly Trees and Dolt-style versioning. Currently alpha, no Rust support, no remote operations, unstable storage format. Not production-ready. Revisit if it matures.

## Consequences

### Positive
- Per-task knowledge isolation — speculative extraction never pollutes canonical store
- Quality-gated promotion — only reviewed knowledge merges to main
- Full history and blame on all knowledge changes
- Safe rollback for bad knowledge merges
- Discarded tasks leave zero residue (branch delete)
- Qdrant provides purpose-built vector search (better than sqlite-vec)
- Single DB for all state (operational simplicity)
- Foundation for future federation (Dolt remotes, DoltHub)

### Negative
- Read latency increases from sub-ms (SQLite in-process) to 1-5ms (MySQL IPC)
- Requires running `dolt sql-server` daemon alongside Djinn
- Qdrant adds a second service to manage
- FTS5 → MySQL FULLTEXT loses BM25 tuning granularity
- Significant migration effort (estimated 6-8 weeks)
- Team must learn Dolt's SQL procedures and branching model
- Compaction/GC adds operational complexity

## Migration / rollout

### Phase 1 — Qdrant sidecar (independent of Dolt)
- Deploy Qdrant alongside existing SQLite
- Replace sqlite-vec with Qdrant for vector search
- Keep all other SQLite operations unchanged
- Validate embedding search quality parity

### Phase 2 — Dolt server and schema migration
- Stand up `dolt sql-server` with health monitoring
- Port schema from SQLite DDL to Dolt-compatible MySQL DDL
- Replace FTS5 triggers with MySQL FULLTEXT indexes
- Migrate refinery migrations to Dolt-compatible format
- Switch `sqlx` pool from SQLite to MySQL protocol
- Data migration: dump SQLite → import to Dolt → verify row counts

### Phase 3 — Per-task branching
- Wire branch creation into coordinator task dispatch
- Wire session connections to check out task branch
- Modify `llm_extraction.rs` to write to task branch
- Build knowledge merge flow (diff → quality gate → selective merge → cleanup)
- Wire branch cleanup into task close/abandon

### Phase 4 — Lifecycle and compaction
- Implement commit compaction in coordinator hourly tick
- Add flatten as scheduled maintenance task
- Configure Dolt GC thresholds
- Monitor commit graph growth

## Relations

- [[ADR-054 Proposal: Memory Extraction Quality Gates and Note Taxonomy]]
- [[ADR-053: Semantic Memory Search — Candle Embeddings with sqlite-vec]]
- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]
- [[ADR-056 Proposal: Planner-Driven Codebase Learning and Memory Hygiene]]
- [[reference/adr-055-sqlite-seam-inventory-for-dolt-migration]] — concrete SQLite/Dolt/Qdrant seam inventory for Wave 1 migration tasks
