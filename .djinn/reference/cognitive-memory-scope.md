# Cognitive Memory Infrastructure Scope

## In Scope

### Phase 11a: Retrieval Pipeline
- `access_count` column on notes + increment on `memory_read`/`touch_accessed`
- `confidence` column on notes (REAL, default 1.0)
- `summary` column on notes (TEXT, auto-generated on write)
- ACT-R temporal priority scoring function (computed at query time)
- Graph proximity scoring with 0.7× hop decay, max 2 hops (uses existing `note_links`)
- Task affinity scoring (uses existing `memory_refs` on tasks)
- RRF fusion of 4 ranked lists with configurable k-constants
- New `memory_search` implementation returning RRF-scored results
- `build_context` upgrade: scored retrieval with progressive disclosure (summaries for related notes, full content for seeds)
- FTS5 field weighting (title=3×, tags=2×, content=1×) — may require FTS5 table recreation

### Phase 11b: Association Learning
- `note_associations` table schema and migration
- Co-access tracking in `NoteRepository::touch_accessed` (session-scoped batch)
- Hebbian weight update on session completion
- Association pruning (weight < 0.05, no co-access in 90 days)
- Implicit associations feed into graph proximity signal
- New `memory_associations` MCP tool to inspect associations for a note

### Phase 11c: Confidence & Contradiction
- Bayesian confidence update function
- Confidence update on task completion (success → +0.65, failure → -0.1)
- Concept-cluster contradiction detection on `memory_write` (FTS overlap check)
- `contradicts` association type with automatic confidence reduction
- Contradiction event emission for desktop notification
- Confidence displayed in search results and note reads

### Phase 11d: Session Reflection
- Post-session reflection job in supervisor (after task completes)
- Extract co-access data from session tool call log
- Batch Hebbian update for all note pairs accessed in session
- Confidence update for notes referenced by completed task
- Access count bulk update
- Event emission for reflection completion

## Out of Scope

- **Vector/semantic search (sqlite-vec)** — separate phase, requires embedding infrastructure. ADR-023 defines it as a future RRF signal.
- **LLM-generated summaries** — future upgrade to simple truncation. Too expensive per-write for now.
- **LLM-based contradiction detection** — structural + concept-cluster only. Semantic analysis deferred.
- **Push-based memory triggers** — requires SSE subscription per agent session. Deferred to post-11 when scale demands it.
- **Predictive Activation Signal (PAS)** — sequential pattern learning. KB too small currently. Revisit at 10K+ notes.
- **Memory defrag agent** — periodic consolidation by LLM. Deferred until KB size warrants it.
- **Citation verification** — checking code references in notes against live codebase. Requires deep git integration. Deferred.
- **Developer persona extraction** — mining git blame/log for agent personas. Separate concern from KB.

## Preferences

- Keep all cognitive logic in SQLite queries where possible — no background workers mutating scores (total-recall design from MuninnDB)
- ACT-R and RRF computed at query time, not stored/cached — deterministic and restartable
- Confidence and association updates happen synchronously in the repository write path (they're cheap SQL UPSERTs)
- Contradiction detection is async (fires after write ACK, doesn't block)
- Association pruning runs on a periodic timer (reuse existing WAL checkpoint timer cadence)
- Existing `memory_search` MCP tool signature preserved — new ranking is transparent to callers
- `build_context` gains optional `max_related` parameter (default 10, previously unlimited)

## Relations
- [[Roadmap]]
- [[ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning]]
- [[Cognitive Memory Systems Research]]
- [[V1 Requirements]]
