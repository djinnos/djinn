# Phase 2 Nightly Non-Gating Memory QA Judge

This runbook documents the nightly Phase 2 memory QA evaluation that runs
after Phase 1 reports land. It is designed to be **non-gating**: it publishes
artifacts and summaries but is never a required PR/merge-queue check and must
never block merges.

## Overview

Phase 2 evaluates whether the real Djinn retrieval/injection pipeline contains
gold answers for questions derived from resolved `pitfall` and `case` notes. It
builds on top of the Phase 1 deterministic benchmark crate
(`server/crates/djinn-memory-eval`) and uses the same fixtures and Postgres test
path, but it adds a credentialed dual-pass LLM judge that only runs in nightly
or manual contexts.

Key files:

| Path | Purpose |
|------|---------|
| `server/crates/djinn-memory-eval/src/qa.rs` | QA pair extraction from pitfall/case notes |
| `server/crates/djinn-memory-eval/src/qa_run.rs` | Deterministic retrieval + 2000-char injection capture |
| `server/crates/djinn-memory-eval/src/qa_judge.rs` | Credential-enforced Phase 2 judge orchestration and failure artifacts |
| `target/memory-eval/phase2-qa-report.json` | Machine-readable Phase 2 report |
| `target/memory-eval/phase2-qa-summary.md` | Human-readable summary for the job summary |
| `.github/workflows/memory-qa-nightly.yml` | Nightly/manual workflow |

## Non-gating status

Phase 2 is intentionally separate from the Phase 1 PR gate:

* Phase 1 is **deterministic, no-LLM, and required** for ranking/eval path PRs.
* Phase 2 is **nightly/manual and informational only**. It may use LLM calls
  and may fail when providers or credentials are unavailable.
* The workflow is triggered by `schedule` or `workflow_dispatch` only; it is
  not wired to `pull_request` or `merge_group`.
* The judge step uses `continue-on-error: true` so that a provider or
credential failure still uploads the Phase 2 artifacts and writes a summary,
without failing the required Quality Gate.

## Credentialed model-slot requirement

Every Phase 2 LLM call must be routed through a **credentialed model slot**,
not an anonymous or default-owner provider resolver. The ojrx lesson is that
background memory LLM calls must be attributed to a real credential; they
must not silently grab any available owner credential or fall back to a shared
default key.

In practice this means:

* The judge command (`cargo run -p djinn-memory-eval -- qa-judge`) must be
  configured with an explicit model slot/provider ID.
* The nightly workflow does **not** provide a default API key or anonymous
  fallback. If the slot is unconfigured, the judge step fails with a clear
  error and the run is recorded as a non-gating failure.
* Per-call costs are recorded in the Phase 2 report with the same cost
  attribution fields as other background LLM sessions: input/output/cache
  tokens, cost-basis (`actual`, `projected`, or `unpriced`), and cost USD or
  an explicit unpriced/null reason.

## Two judge passes and doubled cost

Each QA pair is graded **twice** by independent judge passes (distinct pass
identifiers, and distinct model slot or seed where available). Both passes are
attributed in the report, so the nightly run costs approximately **twice** what
a single-pass judge would cost.

Plan capacity and cost attribution accordingly. The doubled cost is a
deliberate trade-off for measuring inter-judge agreement as a rubric-quality
signal.

## Inter-judge agreement threshold

The Phase 2 report computes the **inter-judge agreement rate**: the fraction
of QA pairs where both passes produced a parseable verdict and the two
verdicts agree (`CORRECT`/`CORRECT` or `WRONG`/`WRONG`).

* Low agreement is treated as a **rubric-quality/variance signal**, not a PR
  gate failure.
* The current agreement target is documented here as a **best-effort threshold**;
  Phase 2 remains non-gating until the maintainers agree that the rubric is
  stable and the threshold is consistently met.
* Do not promote Phase 2 to a required check without explicit board approval
  and a recorded, sustained agreement rate above the threshold.

## Cost attribution fields

The Phase 2 report carries the following cost fields per judge pass and per
run:

| Field | Meaning |
|-------|---------|
| `input_tokens` | Input/prompt tokens consumed by the pass |
| `output_tokens` | Output/completion tokens consumed by the pass |
| `cache_read_tokens` | Prompt cache read tokens, if reported by the provider |
| `cache_write_tokens` | Prompt cache write tokens, if reported by the provider |
| `cost_basis` | `actual` (API-key spend), `projected` (subscription list-rate), or `unpriced` |
| `cost_usd` | Estimated USD cost for the pass, or null/omitted if unpriced |
| `total_cost_usd` | Sum of both passes' costs for the run (includes the doubled cost) |

Unpriced/uncatalogued sessions are recorded visibly but excluded from dollar
aggregates; they are never treated as free.

## Why Phase 2 is separate from Phase 1 baselines

Phase 1 baselines (`baselines/phase1.json`) and the `compare` policy are
**deterministic**. They compare retrieval rank metrics against a committed
baseline and must not depend on LLM calls, provider availability, or day-to-day
judge variance.

Phase 2 adds a **judgmental** layer: it asks whether the retrieved and injected
context actually contains the gold answer. That judgment requires an LLM,
which makes it inherently non-deterministic and too variable for a required PR
gate. Keeping it adjacent but separate preserves Phase 1 gating semantics while
still trending retrieval/injection quality over time.

## Running manually

```bash
# Deterministic QA retrieval/injection capture (no LLM, no network)
cargo run -p djinn-memory-eval -- qa-run

# Dual-pass LLM judge (requires credentialed model slot)
export DJINN_MEMORY_QA_JUDGE_MODEL="<provider>/<model>"
export DJINN_TEST_DATABASE_URL="postgres://postgres:postgres@127.0.0.1:5433/postgres"
cargo run -p djinn-memory-eval -- qa-judge
```

## Failure handling

* If the QA run fails, the workflow fails because the deterministic retrieval
  path is expected to be reliable.
* If the judge fails because of a missing credential or provider error, the
  workflow continues, uploads whatever artifacts exist, and writes a summary
  explaining that the judge was unavailable. This failure is non-gating.
* Do not introduce an anonymous or default-owner fallback to make the judge
  green when credentials are missing.
