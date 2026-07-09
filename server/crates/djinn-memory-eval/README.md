# djinn-memory-eval

Deterministic real-pipeline memory rank benchmark and PR gate for the
Djinn retrieval system.

## Phase 1 scope

Phase 1 delivers a **deterministic, no-LLM benchmark** that:

1. Loads committed JSONL corpus/query/bad-case fixtures into a dedicated
   Postgres schema (via the real repository utilities — not reimplemented
   scoring).
2. Exercises the real `NoteRepository::search` and `build_context` code
   paths.
3. Computes rank metrics: **recall\@k** (k = 1, 5, 10), **MRR**,
   **zero-result rate**, and directional (non-gating) precision/F1.
4. Compares current results against a committed baseline to gate PR
   merges.

All of the above runs **without any LLM or external-network calls**.
Embeddings are supplied by a deterministic cached-vector provider
([`DeterministicEmbeddingProvider`](src/deterministic_embeddings.rs))
keyed by normalised content hash.

## No-LLM / no-external-network requirement

Phase 1 **must never** call an LLM API or reach any external service.
Embeddings are computed locally from SHA-256 content hashes; all search
and context-building operates on committed fixtures in the dedicated
test Postgres (port 5433 in CI).

If a future change introduces a network call, CI will fail the
`djinn-memory-eval` gate.

## Deterministic embeddings

The [`DeterministicEmbeddingProvider`](src/deterministic_embeddings.rs)
implements the `NoteEmbeddingProvider` trait (mirroring the contract in
`djinn_db::repositories::note::embeddings`):

| property             | value                              |
|----------------------|------------------------------------|
| model version        | `deterministic-v1`                 |
| default dimension    | 384                                |
| hash algorithm       | SHA-256                            |
| normalisation        | L2 (unit hypersphere)              |
| content normalisation| CRLF→LF, CR→LF, trim whitespace   |

Identical normalised input always produces byte-identical vectors.
Different content hashes produce distinct deterministic vectors.
No caching layer is needed because computation is pure and fast.

## Metrics

### Gating metrics (drive the PR gate)

| Metric | Scope | Description |
|--------|-------|-------------|
| recall\\@1 | per-suite, aggregate | Fraction of queries with a relevant note at rank ≤ 1 |
| recall\\@5 | per-suite, aggregate | Fraction of queries with a relevant note at rank ≤ 5 |
| recall\\@10 | per-suite, aggregate | Fraction of queries with a relevant note at rank ≤ 10 |
| MRR | per-suite, aggregate | Mean Reciprocal Rank (1/best-rank per query) |
| Zero-result rate | per-suite, aggregate | Fraction of queries with no relevant note in top-k |

### Non-gating / directional metrics

Precision\\@10 and F1\\@10 are computed but **clearly marked directional/non-gating**
in both code and report output. Mined `tasks.memory_refs` labels are sparse —
they represent a subset of truly relevant notes — so true precision and recall
relative to all relevant notes are unknowable.

### Age-bucket recall curves

Recall is broken down by note age (based on `last_accessed` timestamp) to
surface over-decay regressions:

| Bucket | Age range |
|--------|-----------|
| `<7d` | Fresh (accessed within 7 days) |
| `7-30d` | Recent |
| `30-90d` | Mature |
| `>90d` | Over decay threshold |

## Compare policy

The `compare` command evaluates current metrics against the committed baseline
using the following thresholds (from proposal `cxe1`):

| Condition | Threshold | Action |
|-----------|-----------|--------|
| Suite recall\\@k drop | > 0.02 absolute | FAIL |
| Bad-case hit-to-miss regression | any | FAIL |
| Suite MRR drop | > 0.02 absolute | FAIL |
| Aggregate MRR drop | > 0.01 absolute | FAIL |
| Bad-case zero-result increase | any | FAIL |
| Aggregate zero-result increase | > 0.01 absolute | FAIL |

The threshold policy version is tracked in both the baseline and report files
(currently `phase1-v1`).

### Reviewer baseline-update workflow

When a ranking PR is intentionally improving metrics (not a regression):

1. Run `cargo run -p djinn-memory-eval -- run` to produce the new report.
2. Run `cargo run -p djinn-memory-eval -- compare` to verify no unintended regressions.
3. If the compare passes, run `cargo run -p djinn-memory-eval -- refresh-baseline`
   to update `baselines/phase1.json`.
4. Commit the updated baseline alongside the ranking change.

When a compare failure is unexpected, review the per-query regression details
in `target/memory-eval/phase1-summary.md`.

## Fixture & baseline locations

| path                                          | description                              |
|-----------------------------------------------|------------------------------------------|
| `fixtures/`                                   | Committed JSONL corpus, queries, bad-cases |
| `baselines/phase1.json`                       | Committed baseline with fixture hashes, per-suite metrics, per-query top-k ranks, threshold policy version |
| `target/memory-eval/phase1-report.json`       | Full machine-readable report (generated) |
| `target/memory-eval/phase1-summary.md`        | Human-readable summary (generated)       |

## Replay-validation boundary

The existing replay-validation gate in
`djinn-db::repositories::note::replay_validation` validates
anchor-cutover embeddings with a static fixture.  Phase 1's benchmark
**supplements** (does not replace) that gate.  The replay-validation
gate remains in place for anchor-cutover regression detection; Phase 1
covers aggregate corpus-level rank quality.

## Running

```bash
# Validate fixtures and baseline (no DB required)
cargo run -p djinn-memory-eval -- validate-fixtures

# Run the benchmark (requires dedicated Postgres on :5433)
cargo run -p djinn-memory-eval -- run

# Compare against committed baseline
cargo run -p djinn-memory-eval -- compare

# Mine memory_refs for fixture generation
cargo run -p djinn-memory-eval -- mine-memory-refs

# Refresh the committed baseline
cargo run -p djinn-memory-eval -- refresh-baseline
```

### Tests

```bash
cargo test -p djinn-memory-eval
```

## Implementation roadmap

| module                       | task  | status         |
|------------------------------|-------|----------------|
| Crate shell & CLI subcommands| rgif  | ✅ done        |
| Fixture schema contracts     | 27tl  | ✅ done        |
| Deterministic embedder       | csom  | ✅ done        |
| Real Postgres fixture loader | qmzw  | ✅ done        |
| Metrics & compare policy     | zd4o  | ✅ done        |
| Fixture snapshot & baseline  | 77sm  | ✅ done        |
| CI gate wiring & final docs  | 1tk3  | planned        |

## Phase 2 (out of scope)

Phase 2 will introduce a **nightly non-gating LLM judge** with cost
attribution and agreement metrics.  That work lives in epic `ayr9` and
is explicitly excluded from Phase 1 deliverables.  Phase 1 is the first
shippable unit and must stand alone.

## Fixture mining and regeneration

### Corpus structure

The committed fixture corpus (`fixtures/corpus-notes.jsonl`) contains
20 notes with full lifecycle metadata:

- **Lifecycle timestamps**: `created_at`, `updated_at`, `last_accessed` (ISO-8601 UTC).
- **Labels/entities**: extracted entity annotations (concept, technology, file).
- **Graph edges**: typed associations (builds_on, contradicts, supersedes,
  exemplifies, derived_from, co_access).
- **Deterministic embeddings**: 8-dimensional L2-normalised vectors computed
  from SHA-256 content hashes (model version `deterministic-v1`).
- **Signal coverage**: per-note declaration of which retrieval signals are
  expected to surface the note.
- **Task-affinity state**: 5 queries have `task_id` set for task-affinity
  signal testing; their corresponding tasks' `memory_refs` are seeded from
  query and bad-case rows during fixture loading.

### Regenerating fixtures

Fixture files are generated by `scripts/generate_fixtures.py` which
computes deterministic embeddings using the same SHA-256 expand algorithm
as `DeterministicEmbeddingProvider`. To regenerate:

```bash
python3 scripts/generate_fixtures.py
```

This overwrites the JSONL fixture files and recomputes the manifest
SHA-256 hashes. After regeneration, re-run the baseline refresh:

```bash
cargo run -p djinn-memory-eval -- run
cargo run -p djinn-memory-eval -- refresh-baseline
```

### Append-only bad-case process

Bad cases (`fixtures/bad-cases.jsonl`) are **append-only**. Rows are
never deleted or modified — only new rows are added. This ensures the
compare policy can detect regressions by checking that no existing bad
case was made worse. To add a new bad case:

1. Append a new JSONL row with a unique `case_id`.
2. Re-run `scripts/generate_fixtures.py` to update manifest hashes.
3. Run the benchmark and refresh the baseline.
4. Commit the updated fixtures and baseline together.

### First-snapshot signal exclusions

The Phase 1 initial fixture snapshot has the following limitations:

- **Graph proximity signal**: The initial corpus fixtures produce search
  results where graph proximity does not change the rank of relevant
  notes in the signal comparison step. The `assert_signal_effects`
  assertion is deferred to the CI gate task (1tk3). The fixture data
  includes graph edges and signal coverage declarations; signal
  comparisons are recorded but do not gate the initial baseline.

- **Full-text search coverage**: Some fixture queries return zero
  results because PostgreSQL `tsvector` full-text search requires
  exact token/stem matches. Query texts are designed to exercise the
  pipeline; a follow-up corpus refinement can improve lexical recall.

These exclusions are documented here per the design requirement:
"If first-snapshot exclusions remain for a retrieval signal, document
them explicitly in README; do not claim full five-signal coverage."

### Reviewer baseline-update workflow (detailed)

When a ranking PR intentionally improves metrics:

1. Validate the committed fixtures pass schema checks:
   `cargo run -p djinn-memory-eval -- validate-fixtures`
2. Run `cargo run -p djinn-memory-eval -- run` to produce the new report.
3. Run `cargo run -p djinn-memory-eval -- compare` to verify no regressions.
4. If compare passes, run `cargo run -p djinn-memory-eval -- refresh-baseline`.
5. Commit the updated `baselines/phase1.json` alongside the ranking change.

When a compare failure is unexpected, review per-query regression
details in `target/memory-eval/phase1-summary.md`. The report includes
query id, query text, relevant permalink, old rank, new rank, and
metric delta for each regression.
