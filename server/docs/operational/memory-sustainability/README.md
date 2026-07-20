# Memory-sustainability release protocol

This directory is the checked-in operator protocol for the 4 GiB memory-sustainability release gate. It reuses the landed allocator gauges, persisted board scanner, canonical galaxy artifact route, graph-generation identity, and Helm `graphRetention` controls. It does **not** add a production schema, telemetry schema, retention loop, or contract migration.

| Item | Purpose |
|---|---|
| [runbook.md](runbook.md) | Parameterized staging procedure, release evidence, rollout, rollback, and failures |
| [evidence-checklist.md](evidence-checklist.md) | Record to complete and sign before progressing |
| [report-example.md](report-example.md) | Evidence directory layout and reviewable report skeleton |
| `fixtures/` | Deterministic production and reduced smoke fixture generation |
| `driver/` | Production-image collection state machine and append-only JSONL evidence |
| `evaluator/` | Versioned evaluator input contract and machine-readable pass/fail result |
| `smoke.pl` | Cluster-independent fixture + fake-driver + evaluator integration smoke |

## Worker-verifiable smoke

From the repository root, with a writable directory chosen by the caller:

```sh
WORKDIR=/var/tmp/memory-sustainability-smoke
rm -rf "$WORKDIR"
perl server/docs/operational/memory-sustainability/smoke.pl --output-dir "$WORKDIR"
```

The smoke generates the reduced `smoke` fixture, runs the real driver state machine with its synthetic/fake transport and cgroup adapters, translates that collection into the versioned evaluator input, and requires a passing evaluator result. It leaves `fixtures/`, `driver-raw.jsonl`, `evaluator-input.json`, `evaluation.json`, and `evaluation.md` under `$WORKDIR`.

This is a portable worker verification only. It does **not** execute, replace, shorten, or approve the external two-hour production-equivalent staging gate. Follow [runbook.md](runbook.md) and retain the staging artifacts before release approval.
