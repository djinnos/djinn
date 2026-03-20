# Cognitive Memory Infrastructure Scope

## In Scope

### Phase 11a: Retrieval Pipeline & Tiered Abstraction
- `access_count` column on notes + increment on `memory_read`/`touch_accessed`
- `confidence` column on notes (REAL, default 1.0)
- `abstract` column on notes (TEXT, nullable) — LLM-generated L0 (~50-100 tokens)
- `overview` column on notes (TEXT, nullable) — LLM-generated L1 (~300-500 tokens)
- L0/L1 generation on `memory_write` via LLM call (async, note is available immediately with null summaries)
- Backfill job: generate L0/L1 for existing notes on first access or batch migration
- ACT-R temporal priority scoring function (computed at query time)
- Hotness scoring: `sigmoid(log1p(access_count)) × exp_decay(updated_at, 7d)` blended with BM25 (alpha=0.2)
- Graph proximity scoring with 0.7× hop decay, max 2 hops (uses existing `note_links` + `note_associations`)
- Convergence detection: stop BFS after 3 unchanged rounds for top-k
- Task affinity scoring (uses existing `memory_refs` on tasks)
- RRF fusion of 4 ranked lists with configurable k-constants
- New `memory_search` implementation returning RRF-scored results with L0 abstracts
- `build_context` upgrade: budget-aware progressive disclosure (L2 seeds, L1 linked, L0 discovered)
- `build_context` gains `budget` parameter (default 4096 tokens) with priority-tiered allocation
- FTS5 field weighting (title=3×, tags=2×, content=1×) — may require FTS5 table recreation

### Phase 11b: Association Learning
- `note_associations` table schema and migration
- Co-access tracking in `NoteRepository::touch_accessed` (session-scoped batch)
- Hebbian weight update on session completion
- Association pruning (weight < 0.05, no co-access in 90 days)
- Implicit associations feed into graph proximity signal
- New `memory_associations` MCP tool to inspect associations for a note

### Phase 11c: Confidence, Contradiction & Deduplication
- Bayesian confidence update function
- Confidence update on task completion (success → +0.65, failure → -0.1)
- **Two-stage contradiction detection on `memory_write`:**
  - Stage 1: Structural fast-path (FTS overlap check, type compatibility) — synchronous, no LLM
  - Stage 2: LLM semantic analysis for high-overlap candidates — async, sends L0 abstracts + overlapping snippets
  - LLM classifies: `compatible`, `supersedes`, `contradicts`, `elaborates`
- `contradicts` and `superseded_by` association types with automatic confidence reduction
- Contradiction event emission for desktop notification
- **LLM-assisted deduplication on `memory_write`:**
  - BM25 similarity check (>0.8, same type/folder) — most writes skip LLM entirely
  - LLM decides: `skip` (duplicate), `merge` (combine), `keep_both` (distinct)
  - Merge updates existing note content and regenerates L0/L1
- Confidence displayed in search results and note reads

### Phase 11d: Session Reflection (LLM-Assisted)
- Post-session reflection job in supervisor (after task completes)
- **Stage 1 — Structural extraction (no LLM):**
  - Extract co-access data from session tool call log
  - Batch Hebbian update for all note pairs accessed in session
  - Confidence update for notes referenced by completed task
  - Access count bulk update
  - Event taxonomy capture (files, errors, decisions, git ops, tools, notes, tasks)
- **Stage 2 — LLM knowledge extraction (async):**
  - Feed session summary + event log + task description to LLM
  - Extract `case` notes (problem + solution pairs)
  - Extract `pattern` notes (reusable processes)
  - Extract `pitfall` notes (failure modes and fixes)
  - Extracted notes go through normal `memory_write` pipeline (L0/L1, dedup, contradiction)
  - Notes tagged with source session ID, start at confidence 0.5
- Event emission for reflection completion

### Phase 11e: Search Enhancement
- Three-layer fuzzy matching:
  - Layer 1: Porter stemming (existing FTS5 tokenizer)
  - Layer 2: Trigram substring index (new FTS5 table with trigram tokenizer)
  - Layer 3: Levenshtein fuzzy correction (application code, vocabulary index)
- Results from all layers merged before RRF
- Progressive throttling: degrade gracefully under heavy search load (calls 1-3 normal, 4-8 reduced, 9+ blocked)

### Phase 11f: Tool Output Optimization
- `memory_search` defaults to returning L0 abstracts + scores + permalinks (not full content or snippets)
- `build_context` gains `intent` parameter for intent-driven filtering via FTS on matched notes
- Vocabulary hints in search results: top distinctive terms (IDF + identifier bonus) alongside L0 results
- New `memory_batch_query` MCP tool: combine multiple searches + context builds in one call
- Progressive throttling on repeated searches within a session (calls 1-3 normal, 4-8 reduced, 9+ redirect to build_context)
- Smart truncation fallback (60% head + 40% tail) for L1 generation when LLM unavailable

## Out of Scope

- **Vector/semantic search (sqlite-vec)** — separate phase, requires embedding infrastructure. ADR-023 defines it as a future fifth RRF signal.
- **Push-based memory triggers** — requires SSE subscription per agent session. Deferred to post-11 when scale demands it.
- **Predictive Activation Signal (PAS)** — sequential pattern learning. KB too small currently. Revisit at 10K+ notes.
- **Memory defrag agent** — periodic consolidation by LLM. Deferred until KB size warrants it.
- **Citation verification** — checking code references in notes against live codebase. Requires deep git integration. Deferred.
- **Developer persona extraction** — mining git blame/log for agent personas. Separate concern from KB.
- **Intent analysis before retrieval** — OpenViking generates 0-5 typed queries from session context. Overkill for v1; RRF already routes signals.
- **OpenViking adoption** — Python deployment complexity, heavy LLM dependency on every operation. Cherry-pick ideas instead.

## Preferences

- Keep all cognitive logic in SQLite queries where possible — no background workers mutating scores (total-recall design from MuninnDB)
- ACT-R, hotness, and RRF computed at query time, not stored/cached — deterministic and restartable
- Confidence and association updates happen synchronously in the repository write path (they're cheap SQL UPSERTs)
- L0/L1 generation is async (note available immediately, summaries backfilled). Dedup check is synchronous (must block to prevent duplicates).
- Contradiction detection stage 1 is synchronous; stage 2 is async (fires after write ACK)
- Session reflection runs as a background job after task completion
- LLM-extracted notes start at confidence 0.5 (earn trust through usage)
- Association pruning runs on a periodic timer (reuse existing WAL checkpoint timer cadence)
- Existing `memory_search` MCP tool signature preserved — new ranking is transparent to callers
- `build_context` gains optional `budget` parameter (default 4096 tokens)
- Structured LLM prompts with explicit format constraints — no open-ended summarization
- All LLM calls use the cheapest adequate model (haiku-class for L0/L1/dedup/contradiction, not opus-class)

## Relations
- [[Roadmap]]
- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]
- [[Cognitive Memory Systems Research]]
- [[V1 Requirements]]
