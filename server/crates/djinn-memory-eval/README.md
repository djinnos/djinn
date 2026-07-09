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
| Deterministic embedder       | csom  | ✅ this task   |
| Real Postgres fixture loader | qmzw  | planned        |
| Metrics & compare policy     | zd4o  | planned        |
| Fixture snapshot & baseline  | 77sm  | planned        |
| CI gate wiring & final docs  | 1tk3  | planned        |

## Phase 2 (out of scope)

Phase 2 will introduce a **nightly non-gating LLM judge** with cost
attribution and agreement metrics.  That work lives in epic `ayr9` and
is explicitly excluded from Phase 1 deliverables.  Phase 1 is the first
shippable unit and must stand alone.
