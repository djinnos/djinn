# ADR-023: Cognitive Memory Architecture — Multi-Signal Retrieval and Associative Learning

## Status

**Revised v2** — Updated to incorporate LLM-assisted memory operations based on analysis of OpenViking (context database for AI agents) and context-mode (context optimization MCP server). Original v1 avoided all LLM calls in the memory pipeline; v2 adds targeted LLM calls where the per-write cost is recouped many times over through better retrieval quality. See [[Cognitive Memory Systems Research]] and [[OpenViking and Context-Mode Analysis]].

## Context

Djinn's knowledge base currently stores notes as markdown files with a SQLite FTS5 index. Search is BM25-only. The wikilink graph provides manual associations. `last_accessed` is tracked but unused in ranking. `build_context` returns all linked notes without scoring.

This was sufficient when a single human used the KB. It breaks at scale: hundreds of concurrent agents working on thousands of tasks, all reading and writing knowledge simultaneously. Problems:

1. **BM25 drowns in noise** — at thousands of notes, textual relevance alone can't distinguish what matters right now from historical artifacts
2. **Manual wikilinks don't scale** — no human curates thousands of connections; agents don't consistently add `[[wikilinks]]`
3. **No learning from usage** — the system doesn't learn which notes are useful together from the massive implicit signal of agent co-access patterns
4. **No quality signal** — a research note from month one and a battle-tested pattern note used by 200 successful tasks rank equally
5. **Context overload** — dumping all linked notes into an agent's context window degrades performance (ETH Zurich, CodeIF-Bench research; context-mode benchmarks show 98% savings are achievable)
6. **No staleness detection** — notes reference code that changes; stale notes mislead agents
7. **No contradiction handling** — concurrent agents can write conflicting knowledge without detection
8. **Summaries are poor** — first-200-chars truncation loses semantic meaning; agents waste tokens loading full notes to understand what's relevant

Research across MuninnDB, Augment Code, Letta/MemGPT, GitHub Copilot, OpenViking, and context-mode confirms that cognitive retrieval (temporal priority, associative learning, multi-signal fusion, LLM-assisted extraction, tiered abstraction) is essential at multi-agent scale. See [[Cognitive Memory Systems Research]].

### Cost Argument for LLM-Assisted Memory

A task session uses 50K–200K tokens. Memory maintenance per task costs ~6K tokens:

| Operation | Frequency | Token Cost | Savings |
|-----------|-----------|------------|---------|
| L0/L1 summary generation | ~5 notes/task | ~2,500 | Every future retrieval returns compressed context instead of raw content |
| Session reflection | 1/task | ~2,000 | Knowledge that would die with the session is captured permanently |
| Deduplication check | ~1/task | ~500 | Prevents note sprawl that degrades search quality |
| Contradiction check | ~2 high-overlap pairs/task | ~1,000 | Catches semantic conflicts structural detection misses |

That's 3–12% of session cost. One prevented compaction cycle or doom loop iteration pays for it 10×.

Evidence from project research:
- **Explicit-over-implicit pattern**: 100 tokens of good naming saves 1,000–10,000 tokens of agent search. Same principle: 500 tokens of good summary saves thousands of tokens in every subsequent retrieval.
- **Codified context three-tier architecture**: 24.2% knowledge-to-code ratio needed. Quality of that context matters: human-written +4% accuracy, LLM-generated slop -3%. LLM summaries must be structured, not generic.
- **Vertical slice architecture**: Agent effectiveness degrades past 40% context window. Better memory retrieval keeps agents in the effective zone.
- **AI doom loops research**: Similarity-based confusion is root cause. LLM-assisted dedup directly prevents "five nearly-identical notes confusing the agent" failure mode.

## Decision

Upgrade Djinn's knowledge base from a static note store with FTS search to a **cognitive memory system** with multi-signal retrieval, implicit association learning, confidence scoring, LLM-assisted extraction, and tiered context compression. The upgrade is additive — existing markdown-on-disk + SQLite index architecture is preserved.

### 1. Multi-Signal Ranked Retrieval (Reciprocal Rank Fusion)

Replace single-signal BM25 search with a multi-signal pipeline fusing ranked lists via RRF:

```
score(d) = Σ 1/(k_i + rank(d, list_i))
```

**Core signals (v1):**
- **FTS/BM25** (existing, k=60): Textual relevance with field weighting (title=3×, tags=2×, content=1×). Enhanced with three-layer fuzzy matching (inspired by context-mode):
  - Layer 1: Porter stemming via FTS5 tokenizer
  - Layer 2: Trigram substring matching for partial terms
  - Layer 3: Levenshtein fuzzy correction for typos
- **Temporal priority** (new, k=120): ACT-R activation `B(M) = ln(access_count+1) - 0.5 × ln(age_days/(access_count+1))`. Requires new `access_count` column on notes.
- **Hotness scoring** (new, blended with temporal): `sigmoid(log1p(access_count)) × exp_decay(updated_at, half_life=7d)`. Inspired by OpenViking — boosts frequently accessed, recently updated notes. Configurable `HOTNESS_ALPHA` (default 0.2) blends with BM25 score.
- **Graph proximity** (new, k=80): BFS from top-K FTS hits through wikilink graph AND implicit associations, 0.7× decay per hop, max 2 hops. Convergence detection: stop after 3 unchanged rounds for top-k results (from OpenViking).
- **Task affinity** (new, k=100): Notes referenced by the querying task's epic, blockers, or related tasks score higher. Uses existing `memory_refs` on tasks.

**Future signal (separate work):**
- **Vector similarity** via sqlite-vec (DB-08): Semantic search for paraphrase-robust retrieval. When added, becomes a fifth RRF signal.

Final scores multiplied by confidence (default 1.0 for existing notes).

### 2. Tiered Abstraction (L0/L1/L2 Progressive Disclosure)

Inspired by OpenViking's three-tier content model. Replace "first 200 chars" truncation with structured abstractions generated on write:

| Tier | Token Budget | Content | Generated By |
|------|-------------|---------|-------------|
| **L0 (Abstract)** | ~50–100 tokens | One-line purpose + key concepts. Used in search results, catalog, build_context related notes. | LLM on write (cheap, ~200 input + ~100 output tokens) |
| **L1 (Overview)** | ~300–500 tokens | Structured summary: purpose, key points, decisions/conclusions, relations. Used when agent needs more context before committing to full read. | LLM on write (~500 input + ~400 output tokens) |
| **L2 (Full content)** | Unlimited | Complete markdown content. Loaded via `memory_read` when agent needs full detail. | Author (human or agent) |

New columns on notes: `abstract` (TEXT), `overview` (TEXT). Both nullable — existing notes get summaries backfilled on first access or via batch migration.

**Progressive disclosure in build_context:**
1. Seed notes returned at L2 (full content)
2. Directly linked notes returned at L1 (overview)
3. Associated/discovered notes returned at L0 (abstract) with permalink for drill-down
4. Agent calls `memory_read` for any note it wants at L2

**Priority-tiered context budgeting** (inspired by context-mode's snapshot architecture):
- `build_context` accepts a `budget` parameter (default 4096 tokens)
- Budget allocated by tier: seeds get uncapped, L1 notes share 60% of remaining budget, L0 notes share 40%
- Notes within each tier ranked by RRF score; lowest-ranked dropped first when budget exceeded

### 3. Implicit Association Learning (Hebbian)

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

### 4. Confidence Scoring

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

### 5. LLM-Assisted Contradiction Detection

On `memory_write`, two-stage detection:

**Stage 1 — Structural fast-path (no LLM, synchronous):**
1. FTS the new note's content against existing notes in the same project
2. High BM25 overlap (top-3 matches above threshold) = candidate pairs
3. Check note type compatibility (two ADRs on same topic = high risk; ADR + research = low risk)
4. If no candidates pass threshold, done — no LLM call needed

**Stage 2 — LLM semantic analysis (async, only for high-overlap candidates):**
1. For each candidate pair that passes stage 1: send both note abstracts (L0) + the specific overlapping sections to LLM
2. LLM classifies: `compatible` (same topic, no conflict), `supersedes` (new note replaces old), `contradicts` (conflicting claims), `elaborates` (new note extends old)
3. On `contradicts`: create implicit association, lower confidence on both notes, emit event for desktop notification
4. On `supersedes`: lower confidence on older note, create `superseded_by` association, suggest archival

Cost control: only L0 abstracts + overlapping FTS snippets sent to LLM (~300 tokens input). Classification prompt is minimal (~200 tokens output). Stage 1 filters out 90%+ of writes.

### 6. LLM-Assisted Deduplication

On `memory_write`, after contradiction detection:

1. FTS search for notes with >0.8 BM25 similarity to the new note (same type, same folder)
2. If matches found: send new note L0 + top-3 match L0s to LLM
3. LLM decides per match: `skip` (new note is duplicate, don't write), `merge` (combine into existing note), `keep_both` (distinct enough to coexist)
4. On `skip`: return existing note permalink to caller, no write
5. On `merge`: update existing note content, regenerate L0/L1, return permalink
6. On `keep_both`: proceed with normal write

Cost control: only triggered when BM25 finds high-similarity matches in the same type/folder. Most writes have no near-duplicates and skip the LLM call entirely.

### 7. Session Reflection (LLM-Assisted Knowledge Extraction)

After a task session completes, a background job extracts knowledge. Two stages:

**Stage 1 — Structural extraction (no LLM):**
1. Parse session tool call log for note accesses (co-access data → Hebbian update)
2. Track which notes were read but not useful (read without subsequent reference → staleness signal)
3. Update access counts, co-access associations, and confidence scores
4. Capture event taxonomy (inspired by context-mode's 14-category event capture):
   - Files changed, errors encountered, decisions made
   - Git operations performed, tools used
   - Notes read/written, tasks transitioned

**Stage 2 — LLM knowledge extraction (async):**
1. Feed session summary (from compaction) + event log + task description to LLM
2. LLM extracts:
   - **Cases**: Problem + solution pairs from successful tasks (→ `case` note type)
   - **Patterns**: Reusable processes/methods discovered (→ `pattern` note type)
   - **Pitfalls**: Errors encountered and how they were resolved (→ `pitfall` note type)
3. Each extracted note goes through normal `memory_write` pipeline (gets L0/L1, dedup check, contradiction check)
4. Notes tagged with source session ID for provenance

**Memory category taxonomy** (inspired by OpenViking's 6-category model, adapted for Djinn):

| Category | Note Type | Mergeable | Description |
|----------|-----------|-----------|-------------|
| Decisions | `adr` | No | Architecture Decision Records |
| Research | `research` | No | Investigations, analysis, surveys |
| Design | `design` | No | System design documents |
| Patterns | `pattern` | Yes | Reusable solutions, best practices |
| Cases | `case` | Yes | Problem + solution pairs from tasks |
| Pitfalls | `pitfall` | Yes | Known failure modes and fixes |
| Reference | `reference` | No | External pointers, API docs |

Mergeable types can be combined during deduplication. Non-mergeable types get `supersedes` relationships instead.

### 8. Search Enhancement — Three-Layer Fuzzy Matching

Inspired by context-mode's proven FTS5 integration:

**Layer 1 — Porter stemming** (existing FTS5): "caching" matches "cached", "caches". Standard BM25 ranking.

**Layer 2 — Trigram substring** (new): FTS5 trigram tokenizer index for partial term matching. "useEff" finds "useEffect", "coord" finds "coordinator". Separate FTS5 table with trigram tokenizer.

**Layer 3 — Levenshtein fuzzy** (new): For typo correction when layers 1-2 return no results. "kuberntes" → "kubernetes". Computed in application code against a vocabulary index, not in FTS5.

Results from all layers merged into a single ranked list before entering RRF.

### 9. Context-Aware Tool Output Optimization

Inspired by context-mode's core insight: **raw tool output should never enter the agent's context window at full size**. Context-mode benchmarks show 96% savings across 21 real scenarios (376 KB raw → 16.5 KB context). Applied to memory tools:

**Default to L0 in search results:**
- `memory_search` returns L0 abstracts + scores + permalinks, not full content or even snippets
- Agent sees what exists (50-100 tokens per result) and drills down via `memory_read` for L2
- 10 search results at L0: ~500-1000 tokens. Same 10 at full content: ~10,000-50,000 tokens.

**Intent-driven filtering in build_context:**
- `build_context` accepts optional `intent` parameter (e.g., "authentication flow", "database migration patterns")
- When intent provided: RRF results are further filtered to sections matching the intent via FTS
- When no intent: returns standard budget-aware tiered results

**Vocabulary hints in search results:**
- Alongside L0 results, return top distinctive terms from matched notes (IDF + identifier bonus)
- Guides agent to refine queries instead of loading full notes speculatively
- Example: search "auth" returns results + vocabulary `["jwt", "clerk", "middleware", "bearer_token", "session_cookie"]`

**Batch memory operations:**
- New `memory_batch_query` tool combines multiple searches + context builds in one call
- Reduces tool call round trips — an agent exploring a topic makes one call instead of 5-10
- Returns combined results with per-query L0 matches + vocabulary

**Progressive throttling on repeated searches:**
- Calls 1-3: normal results (top-K per query)
- Calls 4-8: reduced results (top-1 per query) + warning suggesting `memory_read` for specifics
- Calls 9+: blocked with redirect to `build_context` (forces the agent to commit to a topic)
- Prevents search loops from consuming the context budget

**Smart truncation fallback for L1:**
- When LLM is unavailable for L1 generation: use 60/40 head+tail truncation (from context-mode)
- Preserves note purpose (first paragraphs) and conclusions (last paragraphs)
- Better than naive "first N chars" which cuts off mid-sentence and loses conclusions

## Consequences

**Positive:**
- Search quality scales with usage — the more agents work, the better retrieval becomes
- No manual curation needed — associations and confidence emerge from agent behavior
- LLM-generated L0/L1 summaries compress context 10-50× vs raw content, keeping agents under the 40% context window threshold
- Session reflection captures institutional knowledge that previously died with agent sessions
- Deduplication prevents note sprawl — the #1 search quality killer at scale
- Contradiction detection catches semantic conflicts that structural detection misses
- Context compression prevents the "junk drawer" problem (Augment research)
- Djinn's unique task-memory coupling becomes a competitive advantage (task outcomes drive confidence)
- All changes are additive — existing FTS search continues working, just ranked better
- Progressive disclosure (L0→L1→L2) minimizes token waste while maximizing agent awareness

**Negative:**
- `note_associations` table grows O(n²) in worst case for heavily co-accessed notes; needs periodic pruning of low-weight entries
- Confidence scoring introduces complexity — bad calibration could suppress useful notes
- RRF adds query latency (4+ ranked lists vs 1), though all are SQLite queries so <50ms total is achievable
- Contradiction detection has false positives (notes on similar topics aren't necessarily contradicting)
- LLM calls on write path add latency (~1-2s) and cost (~6K tokens/task)
- L0/L1 quality depends on LLM quality — bad summaries are worse than no summaries
- Session reflection may extract low-quality notes that pollute the KB

**Mitigations:**
- Confidence decay floor at 0.025 prevents notes from being permanently suppressed
- Association pruning: drop entries with weight < 0.05 and no co-access in 90 days
- Contradiction flags are soft — they lower confidence, not delete notes
- RRF signals can be disabled individually via settings for tuning
- LLM calls are async (don't block the write ACK) except dedup (which must block to prevent duplicates)
- L0/L1 generation uses structured prompts with explicit format constraints, not open-ended summarization
- Session reflection notes start at confidence 0.5 (lower than human-written 1.0) and must earn confidence through successful task usage
- Progressive throttling on search (from context-mode): degrade gracefully under heavy load rather than returning unbounded results

## Relations
- [[Cognitive Memory Systems Research]] — research driving this decision
- [[OpenViking and Context-Mode Analysis]] — reference implementations informing v2 revision
- [[brief]]
- [[roadmap]]
- [[requirements/v1-requirements]]
