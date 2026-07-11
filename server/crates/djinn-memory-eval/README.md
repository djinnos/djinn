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

**Stable bucket keys** (used in JSON reports and baselines):

| Bucket key | Display | Description |
|------------|---------|-------------|
| `under7d` | `<7d` | Notes accessed within 7 days |
| `days7to30` | `7-30d` | Notes accessed 7–30 days ago |
| `days30to90` | `30-90d` | Notes accessed 30–90 days ago |
| `over_decay_threshold` | `>90d` | Notes older than the 90-day decay threshold |

#### Over-decay age-bucket invariant

The committed baseline (`baselines/phase1.json`) **MUST** include the
`over_decay_threshold` age-bucket recall entry when the committed fixtures
include an over-decay-threshold bad case. The `validate-fixtures` command
hard-fails if this invariant is violated.

Age-bucket recall curves include **both** memory-ref query records and
bad-case records so that over-decay fixture cases (e.g. `bc-over-decay-001`
referencing `cases/over-decay-slot-setup`) contribute to the over-decay
recall metrics. This ensures that the >90d bucket is populated whenever
the fixtures contain notes older than the decay threshold.

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
gate remains the **anchor-cutover replay gate** — it is NOT the general
memory-quality benchmark. Phase 1 covers aggregate corpus-level rank
quality across recall/MRR/zero-result dimensions; the replay gate
covers embedding cutover fidelity. The two are complementary: a ranking
change that shifts aggregate recall without touching the embedding
model should pass replay-validation and be judged by Phase 1; an
embedding model change should be judged by both.

## CI integration

The deterministic Phase 1 benchmark is wired as a required PR gate in
`.github/workflows/quality-gate.yml` (the `memory-eval` job). It runs on
PRs and merge-queue submissions that touch the ranking-path files:

- `server/crates/djinn-db/src/repositories/note/search.rs`
- `server/crates/djinn-db/src/repositories/note/rrf.rs`
- `server/crates/djinn-db/src/repositories/note/scoring.rs`
- `server/crates/djinn-db/src/repositories/note/context.rs`
- `server/crates/djinn-memory-eval/**` (fixtures, baseline, code)

The job provisions the same Postgres `:5433` service, applies djinn-db
migrations, and builds the `djinn_test_template` clone database used by
`Database::open_in_memory()` — identical to the `server-test` job setup.
It runs `validate-fixtures`, then `run`, then `compare`. The `compare`
command exits non-zero on any unapproved threshold regression, failing
the job.

**Artifacts and job summary:** the job uploads
`target/memory-eval/phase1-report.json` and
`target/memory-eval/phase1-summary.md` as GitHub Actions artifacts
(retained 30 days) and appends the Markdown summary to the Actions job
summary so it renders inline on the run page. A failed compare includes
per-query regression details (query id, query text, relevant permalink,
old rank, new rank, metric delta) directly in the summary.

**No LLM, no external network:** the benchmark uses deterministic
embeddings and committed fixtures only. If a future change introduces a
network call, CI fails the gate.

### Coordination policy

#### Ranking-behavior PRs own baseline refresh

A PR that intentionally changes ranking behavior **must** refresh
`baselines/phase1.json` in the same commit and include a rationale in
the PR description explaining why the metric change is expected. The
workflow:

1. Run `cargo run -p djinn-memory-eval -- run` (Postgres on `:5433`).
2. Run `cargo run -p djinn-memory-eval -- compare` — expect it to fail
   with the old baseline (that's the change you're shipping).
3. Run `cargo run -p djinn-memory-eval -- refresh-baseline`.
4. Commit `baselines/phase1.json` alongside the ranking change.
5. Document the rationale (which signals changed, expected metric
   direction) in the PR description.

**Unexplained threshold failures block merging.** If `compare` fails and
the PR does not include a refreshed baseline with a rationale, the
Quality Gate stays red.

#### Fixture-only PRs

A PR that only adds or changes fixture rows (queries, bad-cases, corpus
notes) **may** refresh only the added/changed rows in the baseline. It
does not need to re-justify unrelated suite metrics. Append-only bad-case
additions are always safe: they can only add new hit-to-miss checks, not
remove existing coverage.

The fixture manifest hashes (`fixtures/manifest.json`) and baseline
`metadata.fixture_hashes` must be consistent after any fixture change —
the loader validates this.

#### Reviewer checklist for baseline updates

- [ ] Fixture manifest hashes match the committed fixtures.
- [ ] Baseline `metadata.fixture_hashes` match the manifest.
- [ ] If a suite metric dropped, the PR description explains the
      expected direction and which signal caused it.
- [ ] If a bad-case changed from hit to miss without explanation, the
      compare fails and the PR cannot merge until addressed.

#### Worker-facing search caveat

Worker-facing memory search may use a **lexical-only** retrieval path
that differs from the host pipeline exercised by this benchmark. Until
the host ranking pipeline (RRF, scoring, graph, task-affinity) is routed
to the worker-facing search endpoint, metrics from this benchmark
describe the **host** retrieval quality, not necessarily what an
individual worker agent sees. A worker reporting different search
results than the benchmark predicts may be hitting the lexical-only path.

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
| CI gate wiring & final docs  | 1tk3  | ✅ done        |

## Phase 2 (nightly non-gating QA judge)

Phase 2 adds a **nightly non-gating LLM judge** with cost attribution and dual-judge agreement metrics. It lives in epic `ayr9` and is now wired alongside Phase 1 as a separate, non-required workflow. Phase 1 remains the first shippable unit and must stand alone; Phase 2 may only use LLM calls in nightly or manual contexts.

Phase 2 docs are maintained in the [Phase 2 runbook](docs/phase2-runbook.md). Key points:

- **Non-gating only**: the workflow is triggered by `schedule` or `workflow_dispatch` and is never a `pull_request` or `merge_queue` required check.
- **Credentialed model slot**: every judge LLM call must use a credentialed model slot; there is no anonymous or default-owner fallback (ojrx lesson).
- **Dual judge passes**: every QA pair is graded twice, doubling the attributable cost.
- **Inter-judge agreement**: the agreement rate is a rubric-quality/variance signal; Phase 2 remains non-gating until a documented threshold is sustained.
- **Adjacent artifacts**: `target/memory-eval/phase2-qa-report.json` and `target/memory-eval/phase2-qa-summary.md` are produced separately from Phase 1 reports and baselines.

### Phase 2 CLI commands

```bash
# Deterministic QA retrieval/injection capture (no LLM, no network)
cargo run -p djinn-memory-eval -- qa-run

# Dual-pass LLM judge (requires credentialed model slot; nightly/manual only)
export DJINN_MEMORY_QA_JUDGE_MODEL="<provider>/<model>"
export DJINN_TEST_DATABASE_URL="postgres://postgres:postgres@127.0.0.1:5433/postgres"
cargo run -p djinn-memory-eval -- qa-judge
```

See [docs/phase2-runbook.md](docs/phase2-runbook.md) for full details on cost attribution, failure handling, and separation from Phase 1 baselines.

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

### Aggregate metrics scope

The aggregate metrics include **all labeled Phase 1 queries** — both
mined `memory_ref_queries` and append-only `bad_cases` — so
`aggregate_metrics.query_count` always equals the total number of
labeled queries in the committed fixtures. This prevents silent count
mismatches where bad-case rows could be dropped from aggregate
computations without detection.

The `validate-fixtures` command verifies that the baseline's
`aggregate_metrics.query_count` matches the sum of
`memory_ref_queries.len()` + `bad_cases.len()`, and that
`per_query_ranks` covers both suites.

### Minimum useful-baseline expectations

A baseline that passes `validate-fixtures` must:

- Have `aggregate_metrics.query_count` equal to the total fixture
  query count (memory_ref + bad_cases).
- Have `aggregate_metrics.zero_result_rate < 1.0` (not all-miss).
- Have at least one gating retrieval metric (recall@k or MRR)
  greater than zero.
- Include `per_query_ranks` entries for both `all_queries` and
  `bad_cases` suites with counts matching the fixture file lengths.
- Include a `bad_cases` entry in `suite_metrics` when the fixture
  set contains any bad-case rows.
- **Not** be an all-miss baseline: aggregate recall@1/5/10 all zero
  with zero-result-rate 1.0 indicates a broken pipeline or bad fixtures.
  (Test override: `DJINN_MEMORY_EVAL_TEST_OVERRIDE=allow_all_miss_baseline`.)
- Be **produced by `cargo run -p djinn-memory-eval -- run` followed by
  `cargo run -p djinn-memory-eval -- refresh-baseline`** against the
  real `NoteRepository::search` and `build_context` paths — not by
  hand-computing synthetic ranks. The committed `metadata.refresh_commit`
  must equal the git HEAD SHA at the time of refresh so reviewers can
  reproduce the pipeline path that produced the file.  The
  `validate-fixtures` command **rejects** known placeholder values
  (`local-test-refresh`, `unknown`, `placeholder`, `none`, empty) and
  non-hex or too-short commit identifiers.  Committed baselines must
  contain real refresh commit provenance; test-only baseline helpers
  must use explicitly fake hex strings (e.g. `aabbccdd0011…`) rather
  than sharing the production placeholder constants.

The committed `baselines/phase1.json` was refreshed from a live run on
2026-07-10 against the dedicated Postgres test cluster. Its
`metadata.refresh_commit` field carries that run's HEAD SHA so reviewers
can identify the exact ranking commit that produced the rank data.

### Self-checking baseline invariants

The `validate-fixtures` command enforces the following invariants that
prevent the specific regressions found during planning:

1. **Minimum fixture counts**: at least 25 total labeled queries, at
   least 15 mined `memory_ref_queries`, and at least 10 `bad_cases`.
   A committed fixture set below these thresholds fails validation.
2. **Baseline fixture coverage**: the baseline's
   `aggregate_metrics.query_count` must equal the sum of
   `memory_ref_queries.len()` + `bad_cases.len()`. The baseline's
   `per_query_ranks` must cover both suites with counts matching the
   fixture file lengths. Bad-case rows **must not** be silently
   dropped from aggregate computations.
3. **Suite metrics completeness**: when fixtures contain bad-case rows,
   the baseline's `suite_metrics` must include a `bad_cases` key.
4. **No all-miss baselines**: aggregate recall@1/5/10 all zero with
   zero-result-rate 1.0 is rejected. This catches broken pipelines,
   meaningless fixtures, or queries that don't match the corpus. Each
   non-empty suite is individually checked as well.
5. **Hard signal coverage**: fixture validation fails (not warns) when
   a retrieval signal is claimed on a fixture row but the required
   supporting data is missing — graph signal without graph edges,
   entity signal without labels, vector signal without embeddings,
   or task-affinity signal without a task_id. These are hard errors
   in `validate_fixtures`, not warn-only assertions.
6. **Cross-reference integrity**: every memory_ref permalink, graph
   edge endpoint, and bad-case reference must exist in the corpus.
   Manifest counts must match actual fixture lengths.
7. **Real refresh commit provenance**: `metadata.refresh_commit` must
   be a valid hex commit SHA (7+ characters), not a placeholder such
   as `local-test-refresh`, `unknown`, or empty.  The
   `validate-fixtures` command rejects these at load time.

Fixture updates must preserve meaningful retrieval and hard
graph/entity + task-affinity rank-change coverage before refreshing
`baselines/phase1.json`. Adding rows without refreshing the baseline
is safe (append-only); removing or modifying existing rows requires a
baseline refresh with the updated fixture hashes.

### Fixture query-text constraints for retrieval

The committed `fixtures/memory-ref-queries.jsonl` and
`fixtures/bad-cases.jsonl` rows use **concise keyword queries** rather
than verbose natural-language questions. The lexical search path in
`djinn-db::repositories::note::lexical_search` builds a `to_tsquery`
expression by tokenizing the query text and AND-joining all tokens
(with `:*` prefix matching for tokens ≥ 3 chars). When verbose natural
language ("How to handle slot lifecycle race conditions?") is fed to
that pipeline, common English filler words ("how", "handle") have no
matching stems in the fixture corpus, so the strict AND query returns
zero candidates and the search fails to surface any relevant note.

For the real-pipeline baseline to be non-all-miss, fixture queries must
use terms that actually appear in the committed corpus notes. Examples
of the rewrite pattern:

| Verbose form                          | Concise fixture form           |
|---------------------------------------|--------------------------------|
| "How to handle slot lifecycle race..."| "slot lifecycle race"          |
| "Supervisor guard pattern..."          | "supervisor guard"             |
| "Memory note decay tuning and..."     | "memory decay"                 |
| "What API methods are available..."    | "slot api"                     |

The minimum useful-baseline expectations above require the queries to
match at least one term per expected note via the real pipeline. Future
fixture expansions should follow the same concise-keyword convention
or commit to extending the lexical-search tokenizer (e.g. websearch
operator) — the current fixture queries are deliberately short so the
Phase 1 baseline is anchored on **real** pipeline recall, not synthetic
plausibility.

### Required Phase 1 signal invariants

Both **graph/entity** and **task-affinity** rank-change proof cases are
**hard Phase 1 invariants**. The run, baseline validation, and PR gate
all require:

1. At least one graph/entity signal comparison with `rank_changed=true`
   in `signal_comparisons`.
2. At least one task-affinity signal comparison with `rank_changed=true`
   in `signal_comparisons`.

`assert_signal_effects` (called by `run`) **hard-fails** — not warns —
when either required comparison family is absent or when no comparison
in the family shows a rank change. Committed-baseline validation
likewise rejects `baselines/phase1.json` if it lacks at least one
changed graph/entity comparison and at least one changed task-affinity
comparison.

This prevents silent collapse to lexical/vector/temporal-only retrieval
behavior. Fixture data must include graph edges, entity labels, and
task-affinity memory_refs that produce observable rank changes in the
signal comparison step.

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
