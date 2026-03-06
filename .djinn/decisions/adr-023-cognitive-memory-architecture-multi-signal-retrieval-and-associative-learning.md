# ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning

## Context

Djinn's knowledge base currently stores notes as markdown files with a SQLite FTS5 index. Search is BM25-only. The wikilink graph provides manual associations. `last_accessed` is tracked but unused in ranking. `build_context` returns all linked notes without scoring.

This was sufficient when a single human used the KB. It breaks at scale: hundreds of concurrent agents working on thousands of tasks, all reading and writing knowledge simultaneously. Problems:

1. **BM25 drowns in noise** — at thousands of notes, textual relevance alone can't distinguish what matters right now from historical artifacts
2. **Manual wikilinks don't scale** — no human curates thousands of connections; agents don't consistently add `[[wikilinks]]`
3. **No learning from usage** — the system doesn't learn which notes are useful together from the massive implicit signal of agent co-access patterns
4. **No quality signal** — a research note from month one and a battle-tested pattern note used by 200 successful tasks rank equally
5. **Context overload** — dumping all linked notes into an agent's context window degrades performance (ETH Zurich, CodeIF-Bench research)
6. **No staleness detection** — notes reference code that changes; stale notes mislead agents
7. **No contradiction handling** — concurrent agents can write conflicting knowledge without detection

Research across MuninnDB, Augment Code, Letta/MemGPT, and GitHub Copilot confirms that cognitive retrieval (temporal priority, associative learning, multi-signal fusion, contradiction detection) is essential at multi-agent scale. See [[Cognitive Memory Systems Research]].

## Decision

Upgrade Djinn's knowledge base from a static note store with FTS search to a **cognitive memory system** with multi-signal retrieval, implicit association learning, confidence scoring, and context compression. The upgrade is additive — existing markdown-on-disk + SQLite index architecture is preserved.

### 1. Multi-Signal Ranked Retrieval (Reciprocal Rank Fusion)

Replace single-signal BM25 search with a multi-signal pipeline fusing four ranked lists via RRF:

```
score(d) = Σ 1/(k_i + rank(d, list_i))
```

**Signals:**
- **FTS/BM25** (existing, k=60): Textual relevance with field weighting (title=3×, tags=2×, content=1×)
- **Temporal priority** (new, k=120): ACT-R activation `B(M) = ln(access_count+1) - 0.5 × ln(age_days/(access_count+1))`. Requires new `access_count` column on notes.
- **Graph proximity** (new, k=80): BFS from top-K FTS hits through wikilink graph, 0.7× decay per hop, max 2 hops
- **Task affinity** (new, k=100): Notes referenced by the querying task's epic, blockers, or related tasks score higher. Uses existing `memory_refs` on tasks.

**Future signal (separate work):**
- **Vector similarity** via sqlite-vec (DB-08): Semantic search for paraphrase-robust retrieval

Final scores multiplied by confidence (default 1.0 for existing notes).

### 2. Implicit Association Learning (Hebbian)

New `note_associations` table:
```sql
CREATE TABLE note_associations (
    note_a_id TEXT NOT NULL,
    note_b_id TEXT NOT NULL,
    weight REAL NOT NULL DEFAULT 0.01,
    co_access_count INTEGER NOT NULL DEFAULT 1,
    last_co_access TEXT NOT NULL,
    PRIMARY KEY (note_a_id, note_b_id),
    CHECK (note_a_id < note_b_id)  -- canonical ordering
);
```

When an agent session reads multiple notes, all pairs are recorded as co-accesses. Weight update: `w_new = min(1.0, w_old × (1 + 0.01)^n)` where n = co-access count in this session. Canonical pair keys `(min(a,b), max(a,b))` ensure deduplication.

Implicit associations feed into the graph proximity signal alongside explicit wikilinks, with lower initial weight (0.01 vs 1.0 for wikilinks).

### 3. Confidence Scoring

New `confidence` column on notes (REAL, default 1.0, range [0.025, 0.975]).

**Evidence signals that increase confidence:**
- Task referencing the note completes successfully: +0.65 signal
- Note co-accessed with high-confidence notes: +0.65 signal
- User explicitly confirms note: +0.95 signal

**Evidence signals that decrease confidence:**
- Contradiction detected with another note: 0.1 signal
- Task referencing the note fails or is reopened multiple times: 0.1 signal
- Note flagged as stale by citation check: 0.3 signal

Bayesian update: `posterior = (p×s) / (p×s + (1-p)×(1-s))`, with Laplace smoothing.

### 4. Context Compression in build_context

Replace "return all linked notes" with scored retrieval:
1. Seed note(s) loaded at full content
2. Linked/associated notes scored via multi-signal pipeline
3. Top-K returned with **summaries only** (first 200 chars + title) instead of full content
4. Agent can `memory_read` specific notes it wants full content for

This implements progressive disclosure (Letta pattern): agents see what exists, load what they need.

### 5. Contradiction Detection (Structural + Concept-Cluster)

On `memory_write`:
1. FTS the new note's content against existing notes in the same project
2. High BM25 overlap (top-3 matches above threshold) = candidate pairs
3. For candidates: check if note types are compatible (two ADRs on the same topic = potential contradiction; an ADR and a research note = less likely)
4. Flag contradiction: create `contradicts` implicit association, lower confidence on both notes, emit event for desktop notification

No LLM-based semantic analysis (too expensive for every write). Structural + concept-cluster detection only.

### 6. Session Reflection (Post-Task Knowledge Extraction)

After a task session completes successfully:
1. Background job reviews the session's tool call log (notes read, files changed, patterns used)
2. Extracts: which notes were useful (co-access data → Hebbian update), which notes were stale (read but not useful), what new knowledge was created
3. Updates access counts, co-access associations, and confidence scores
4. Optionally writes new pattern/pitfall notes from session findings (future: LLM extraction)

### 7. Note Summaries

New `summary` column on notes (TEXT, nullable). Auto-generated on write as first ~200 characters of content (simple truncation). Used by:
- `build_context` for progressive disclosure
- Search results (richer than FTS snippets)
- Catalog generation (optional descriptions)

Future: LLM-generated summaries for higher quality.

## Consequences

**Positive:**
- Search quality scales with usage — the more agents work, the better retrieval becomes
- No manual curation needed — associations and confidence emerge from agent behavior
- Context compression prevents the "junk drawer" problem (Augment research)
- Contradiction detection catches conflicting knowledge before agents act on it
- Djinn's unique task-memory coupling becomes a competitive advantage (task outcomes drive confidence)
- All changes are additive — existing FTS search continues working, just ranked better

**Negative:**
- `note_associations` table grows O(n²) in worst case for heavily co-accessed notes; needs periodic pruning of low-weight entries
- Confidence scoring introduces complexity — bad calibration could suppress useful notes
- RRF adds query latency (4 ranked lists vs 1), though all are SQLite queries so <50ms total is achievable
- Contradiction detection has false positives (notes on similar topics aren't necessarily contradicting)

**Mitigations:**
- Confidence decay floor at 0.025 prevents notes from being permanently suppressed
- Association pruning: drop entries with weight < 0.05 and no co-access in 90 days
- Contradiction flags are soft — they lower confidence, not delete notes
- RRF signals can be disabled individually via settings for tuning

## Relations
- [[Cognitive Memory Systems Research]] — research driving this decision
- [[Project Brief]]
- [[Roadmap]]
- [[V1 Requirements]]
