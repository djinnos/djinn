# Offline extraction replay gate

The extraction replay gate is the repository-supported regression check for post-session knowledge extraction. It reuses the production parser, ADR-054 quality gate, and novelty/dedup decision contract, but stops before note persistence.

## Prerequisites

Run from `server/`. The command requires the dedicated **test** Postgres service, not a production database. `TEST_POSTGRES_URL` (or `DJINN_TEST_DATABASE_URL`) must point at that service and it must contain the migrated `djinn_test_template` template database. The normal test-image/template bootstrap is responsible for creating this template; do not point this command at `DJINN_DATABASE_URL` or an operator memory database.

The fixture corpus is committed at `crates/djinn-slot/tests/fixtures/extraction_replay/`. It is validated before a database is created: IDs, safe provenance, transcript shape, note type, rubric satisfiability, and prohibited secret/endpoint data are checked.

## Run the deterministic gate

```sh
cd server
TEST_POSTGRES_URL="$TEST_POSTGRES_URL" \
  cargo run -p djinn-slot --features test-support --bin extraction-replay
```

The default command creates a template-cloned throwaway Postgres database, seeds only an evaluation project, persists fixture transcript rows, loads them through `SessionMessageRepository`, and uses queue-backed injected fixture providers. It neither resolves an ambient provider client nor reads provider credentials, production memory, or a production project. It makes no network request.

Stable outputs are overwritten on each run:

- `server/target/extraction-replay/report.json` — machine-readable deterministic report.
- `server/target/extraction-replay/report.md` — per-fixture PASS/FAIL dimensions plus aggregate rubric satisfaction, dedup confusion (`TP`, `FP`, `TN`, `FN`), and precision.

The command exits non-zero if either configured floor is not satisfied. Defaults are both `1.0`; pass reviewed temporary floors explicitly when investigating a proposed change:

```sh
cargo run -p djinn-slot --features test-support --bin extraction-replay -- \
  --minimum-rubric 0.95 --minimum-dedup-precision 0.98 \
  --output-dir target/extraction-replay/candidate
```

A lower floor is not a baseline update. Attach the candidate JSON/Markdown to review, identify every changed fixture/dimension, and obtain extraction-owner review before changing the committed default policy or accepting a lower baseline. Restoring a floor requires the same evidence.

## Required change workflows

Run the offline gate and review both reports whenever changing:

1. an extraction prompt or terminal-outcome prompt context;
2. an extraction quality/admission threshold or ADR-054 classifier behavior;
3. novelty, candidate lookup, dedup request/response handling, or duplicate policy;
4. the fixture corpus or its annotations; or
5. future extraction revision-operation behavior.

For fixture changes, keep 20–50 sanitized archived-transcript-derived rows and rerun the validation plus replay command. For revision operations, extend this same report/fixture model and production-path capture seam; do not create a parallel evaluator.

## Provider boundary and non-goals

This binary intentionally exposes **no real-provider mode**. Consequently it has no ambient default client fallback and cannot consume ambient credentials. If a future explicitly opted-in real-provider command is added, startup must require all of: an explicit provider, explicit model, explicit credential source, and a non-empty task/run (or equivalent) cost-attribution identity. Missing any one must fail closed before provider construction; silent defaults are prohibited.

Online A/B testing, live traffic experimentation, and policy-training frameworks are out of scope. This gate is an offline deterministic regression harness only.
