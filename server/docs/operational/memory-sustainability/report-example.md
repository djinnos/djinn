# Memory-sustainability evidence report example

Use this as the release-record index. Values below are placeholders, not a claim that a staging gate passed.

```text
/durable/release-evidence/memory-sustainability/CHG-1234/
├── candidate/
│   ├── image-digest.txt
│   ├── started-at.txt
│   ├── finished-at.txt
│   ├── server-pods-before.txt
│   ├── restart-baseline.txt
│   ├── fixtures/
│   │   ├── fixture-report.json
│   │   ├── canonical-graph.blob
│   │   ├── board-health-tasks.jsonl
│   │   └── galaxy-artifact/
│   ├── fixture-generation.json
│   ├── fixture-manifest.json
│   ├── fixture-checksums.sha256
│   ├── driver-raw.jsonl
│   ├── driver.log
│   └── raw-checksums.sha256
├── pre-change-diagnostic/
│   ├── image-digest.txt
│   ├── driver-raw.jsonl
│   └── driver.log
├── evaluator-input.json
├── evaluation.json                 # machine-readable decision
├── evaluation.md                   # exact human rendering
├── evaluator.log
├── evaluation-checksums.sha256
├── retention-dry-run-helm.txt
└── retention-delete-helm.txt       # absent unless delete was approved
```

## Release record

| Field | Recorded value |
|---|---|
| Change ID | `CHG-1234` |
| Candidate digest | `registry.example/djinn-server@sha256:<digest>` |
| Pre-change digest | `registry.example/djinn-server@sha256:<digest>` |
| Candidate / diagnostic run IDs | `<candidate-run-id>` / `<diagnostic-run-id>` |
| Fixture profile, seed, manifest SHA-256 | `production`, `ste6-production-v1`, `<sha256>` |
| Cgroup identity and `memory.max` | `<pod>/<container>/<cgroup-path>`, `4294967296` |
| Graph generation | `<generation-id>` unchanged from install through T2 |
| Candidate timestamps | `<started UTC>` through `<finished UTC>` |
| OOM / restart baseline and delta | `<baseline> -> <final> (0)` / `<baseline> -> <final> (0)` |
| Raw evidence | `candidate/driver-raw.jsonl`, SHA-256 `<sha256>` |
| Evaluator input/result | `evaluator-input.json` / `evaluation.json`, SHA-256 `<sha256>` |
| Candidate status | `pass` or `fail` — copied from `evaluation.json` |
| Diagnostic status | `<status>` (explicitly non-gating) |
| Reviewer / approver | `<name + timestamp>` / `<name + timestamp>` |
| Rollback decision | `advance`, `hold`, or `rollback`; include operator and timestamp |

## Gate result excerpt

Paste the generated evaluator summary, not a transcription of dashboard values:

```text
Candidate release status: PASS
server_peak: pass; warm_job_peak: pass; route_rss_delta: pass
board_pass_duration: pass; oom_delta: pass; restart_delta: pass
same_graph_generation: pass; t2_rss_delta: pass
t2_jemalloc_retained_delta: pass
```

Attach the full `evaluation.json`; it retains raw measurements and JSON-pointer evidence references. A nonzero evaluator exit, missing raw artifact, changed generation, OOM/restart delta, or a failed check records a failed candidate and requires the rerun/failure procedure in [runbook.md](runbook.md).

## Contract boundary

Record legacy telemetry count and rollback-window close separately. Even a passing evaluation and retention delete are **not** authorization to contract/drop. This epic performs no contract migration: it is prohibited until zero legacy telemetry is observed and the rollback window is closed.
