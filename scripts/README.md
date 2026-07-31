# Scripts

## Task-run backstop full validation

`validate-taskrun-backstop.sh` is the epic 8451 full Rust validation entrypoint for hosts that have Docker/Postgres available. It starts the repo's `postgres-test` service, rebuilds `djinn_test_template`, creates the test vault key, then runs `cargo build`, strict workspace clippy, and full workspace nextest.

```sh
make validate-taskrun-backstop
# or directly:
./scripts/validate-taskrun-backstop.sh
```

Required tools for the direct script: Docker with Compose, Cargo, `cargo-nextest`, `sqlx-cli`, and OpenSSL. The Makefile target additionally requires Make. Logs are written under `.taskrun-backstop-validation/`, which is gitignored.

## Task-run backstop operator preflight

`taskrun-backstop-preflight.sh` captures the operator/admin prerequisites needed before running `docs/TASKRUN_BACKSTOP_VERIFICATION.md`. It checks `kubectl`, current context/namespace, Pod/Job read RBAC, `deploy/djinn-server` log access, and Djinn MCP/control-plane authentication without killing or force-closing a task.

```sh
NS=djinn \
  DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp" \
  DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>" \
  ./scripts/taskrun-backstop-preflight.sh | tee taskrun-backstop-preflight.md
```

Paste the generated Markdown bundle into `docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md` before collecting kill/force-close cleanup evidence, after redacting secrets.

## Task-run backstop operator evidence runner

`taskrun-backstop-e2e-evidence.sh` is the Wave 3 operator/admin evidence runner. It wraps the preflight plus the kill/force-close verification steps from `docs/TASKRUN_BACKSTOP_VERIFICATION.md` into a single auditable Markdown bundle that can be pasted straight into `docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md`.

The runner captures:

- the embedded preflight output from `taskrun-backstop-preflight.sh` (kubectl, context/namespace, Pod/Job RBAC, server log access, authenticated MCP `initialize`);
- before-action `kubectl get jobs,pods` evidence filtered by the task-run label and the canonical `djinn-taskrun-$TASK_RUN_ID` prefix;
- the exact `execution_kill_task` (or force-close/operator-close) action placeholder the operator is expected to issue from the same shell;
- a 60-second post-action polling loop checking both the task-run label and the canonical Job/Pod name prefix;
- `kubectl logs` for `deploy/djinn-server` filtered around task-run/backstop markers (`task-run Job backstop`, `backstop reaped orphaned task-run Job`, `task_run_id=...`, `job_name`).

It redacts the operator bearer token, records task id, task run id, namespace/context, UTC timestamps, exact commands, and exit statuses, and fails closed (non-zero exit) when required inputs/access are missing. It does **not** claim cleanup success unless the preflight passes, the operator marks the bundle as `action=executed` (`ACTION_RESULT` set), and the 60-second post-action poll converges.

```sh
# Kill bundle (execution_kill_task).
NS=djinn \
  TASK_ID="<long-running-task-id>" \
  TASK_RUN_ID="<active-task-run-id>" \
  DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp" \
  DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>" \
  MODE=kill \
  ACTION_INVOKED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  ACTION_RESULT="execution_kill_task returned ok" \
  ./scripts/taskrun-backstop-e2e-evidence.sh | tee taskrun-backstop-e2e-kill.md

# Force-close bundle (operator/admin force-close or proposal abort).
NS=djinn \
  TASK_ID="<long-running-task-id>" \
  TASK_RUN_ID="<active-task-run-id>" \
  DJINN_MCP_URL="https://<operator-accessible-djinn-host>/mcp" \
  DJINN_OPERATOR_BEARER_TOKEN="<operator/admin token>" \
  MODE=force-close \
  ACTION_INVOKED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  ACTION_RESULT="proposal abort 4711 closed task" \
  ./scripts/taskrun-backstop-e2e-evidence.sh | tee taskrun-backstop-e2e-force-close.md
```

Set `DRY_RUN=1` to emit the same bundle shape with the action placeholder only — useful for rehearsing the paste workflow without invoking anything. Redact bearer tokens, kubeconfig client keys/certificates, cookies, database URLs, and full health payloads containing credentials before committing the bundle.

See `docs/TASKRUN_BACKSTOP_VERIFICATION.md` (Wave 3 operator evidence runner section) and `docs/TASKRUN_BACKSTOP_E2E_EVIDENCE.md` (Wave 3 paste points) for the exact usage and paste location.

## JIT pitfall effectiveness readout bundle

`jit-pitfall-readout-bundle.sh` emits a local, operator-fillable Markdown bundle for the JIT pitfall cohort effectiveness read in `docs/JIT_PITFALL_EFFECTIVENESS_READ.md`. It is a template helper only: default mode (`DRY_RUN=1`) does not connect to production, query telemetry, or read raw logs. Operators fill the generated placeholders from safe, already-redacted telemetry summaries.

```sh
./scripts/jit-pitfall-readout-bundle.sh | tee jit-pitfall-readout.md

# Optional safe scalar/reference metadata can pre-fill a few rows:
READOUT_ID="2026-06-cohort-a" \
  ENVIRONMENT="staging/djinn" \
  COHORT_RULE="staging namespace cohort" \
  ROLLOUT_WINDOW_UTC="2026-06-16T00:00:00Z/2026-06-17T00:00:00Z" \
  TELEMETRY_COUNTERS_REF="redacted/jit-counter-summary.md" \
  EFFECTIVENESS_REF="redacted/jit-effectiveness-summary.md" \
  NOISE_SAMPLE_REF="redacted/jit-noise-summary.md" \
  PROMPT_BUDGET_REF="redacted/jit-prompt-budget-summary.md" \
  ./scripts/jit-pitfall-readout-bundle.sh | tee jit-pitfall-readout.md
```

The bundle mirrors the readout sections that must be completed before a planner may consider a default-on flip: rollout record, telemetry counter outcomes, injected-vs-control outcome comparison, empty/error/disabled checks, false-positive/noise sampling, prompt-budget evidence, and the positive-read recommendation/default-on gate.

Safety boundary: do **not** pass or paste raw prompt text, patch/source contents, source file contents, raw prompt logs, or full rendered JIT hint bodies (including `<relevant-pitfalls>...</relevant-pitfalls>`). Accepted inputs are limited to safe counts, rates, operational identifiers, rollout metadata, note ids/permalinks/types/ranks/confidence buckets, bounded path summaries, short operator classifications, and paths/links to already-redacted summaries.

Minimal local check:

```sh
./scripts/jit-pitfall-readout-bundle.sh --self-test
```

## Rust size report

Measures Rust source files under `server/crates/**` and `server/src/**` and ranks the largest worst-first. **It does not fail.** The only non-zero exit is a usage error.

### Why it is not a gate

It was one, from 2026-06-11 to 2026-07-30. The outcome over those seven weeks:

| | at landing | 2026-07-30 |
| --- | --: | --: |
| files carrying `// djinn:allow-oversize` | 0 | 108 |
| oversized files (by line count) | 22 | 79 |
| files split in response | — | 0 |

101 of the 108 marker-carrying files were genuinely oversized and **no** oversized file lacked a marker. Every file that ever tripped the gate got a marker; none got restructured.

That is an incentive gap, not a discipline problem: complying means restructuring a module, evading means adding one comment line. At roughly 100x cost asymmetry, evasion always wins. A per-file *line* limit is also satisfiable by cutting a file at any point, and the `_partN` fragments that produced hid 20.7k lines from rust-analyzer, rustfmt and three CI guards.

On 2026-07-30 the gate blocked two correctness fixes on `MAX_BYTES`: PR #2815 (141 bytes of headroom, tipped over by its own explanatory comment) and PR #2839 (267 bytes, tipped over by the coverage fix itself). Both were resolved by adding a marker.

Pressure toward workable file sizes now lives at authorship time instead, in `djinn-agent`'s `edit`/`write`/`apply_patch` tool results — the moment where there is nothing to satisfy and therefore nothing to game.

### CI

`.github/workflows/quality-gate.yml` runs the "Report changed Rust file sizes" step of the `server-guards` job for PR and merge-queue server changes. The step computes added, modified, and renamed files with `git diff --name-only --diff-filter=AMR` and pipes that list to changed-file mode:

```sh
./scripts/check-file-size.sh --files-from-stdin
```

The step is structurally incapable of failing the run. The ranked table lands in the job step summary and the worst offenders become `::notice` annotations (capped at 10, which is all GitHub renders per step). CI covers new or edited Rust files; it does not full-tree scan every legacy file on each PR.

### Run locally

Changed-file mode, matching CI input style:

```sh
printf '%s\n' server/crates/foo/src/lib.rs | ./scripts/check-file-size.sh --files-from-stdin
```

Full-tree audit mode:

```sh
./scripts/check-file-size.sh --all
```

### Thresholds

Defaults are `MAX_LINES=1500` and `MAX_BYTES=51200`. These now decide only what appears in the report, not what is permitted. Override either value with environment variables:

```sh
MAX_LINES=1200 MAX_BYTES=45000 ./scripts/check-file-size.sh --all
```

Rows are ranked by **bytes** descending, because bytes is the binding constraint on whether a reader can load a file at all — an agent's `read` returns at most 2000 lines and at most `output_stash::MAX_TOOL_RESULT_CHARS` (30 000) characters. Line count is shown alongside as the more legible number.

### The marker

`// djinn:allow-oversize` anywhere in a file no longer grants anything, because nothing is denied. It is still parsed: it is the author's on-the-record statement that the file's size is deliberate, and it splits the ranking into `acknowledged` and `unacknowledged` rows — the one axis on which the report is actionable.

The 108 existing markers are deliberately left in place. Removing them would be 108 files of churn for zero behavioural change; if that cleanup is ever wanted it is its own piece of work.

### Skipped paths

Generated Rust files are skipped defensively: paths matching `**/generated/**` and files matching `*.gen.*`.

### Tests

```sh
sh scripts/test-check-file-size.sh
```

## Phase 1 retirement deletion guard

`djinn-retirement-manifest.mjs` generated and validated the pre-cutover
knowledge manifest. The validated result is retained permanently as
`scripts/fixtures/djinn-retirement/deletion-ledger.json`. Each ledger entry
records its repository path, exact blob and normalized-content SHA-256 hashes,
permalink, selected DB UUID/permalink/status/hash, rationale, approval, and one
resolved disposition (`equivalent`, `db_supersedes_file`, or
`approved_discard`). The ledger also records the immutable source commit so its
blobs remain independently auditable after deletion.

Normalization only removes YAML front matter and canonicalizes CRLF/CR to LF;
committed blob SHA-256 is computed over the exact stored bytes.

### Hermetic fixtures

Committed under `scripts/fixtures/djinn-retirement/`:

- `deletion-ledger.json` — durable, deterministic evidence for the exact deleted
  knowledge-path set.
- `db-selection.json` — the selected DB identity for each detected permalink.
- `db-guidance.json` — reconciled DB guidance classifications, dispositions,
  rationales, hashes, statuses, and supersession links.

Fixture regeneration is deliberately anchored to the source revision recorded
in the ledger; live HEAD has no tracked knowledge paths after cutover:

```sh
node scripts/fixtures/djinn-retirement/generate.mjs --revision <ledger-source-sha>
```

### Strict post-cutover guard

```sh
make check-retirement-manifest
# or directly:
./scripts/check-djinn-retirement-manifest.sh
```

The guard validates the durable ledger against source-revision blobs and the DB
guidance fixture, including uniqueness, hashes, identities, statuses,
classifications, dispositions, rationales, approvals, count, and exact source
set. It separately classifies all NUL-delimited tracked paths and scans each exact
staged index blob for path-shaped filesystem, constructor, join, and generated
configuration surfaces; ordinary historical prose remains allowed. It fails for every
tracked project-local knowledge path, including paths never
present in the old ledger. The retired operational files `.djinn/.gitignore`,
`.djinn/settings.json`, and `.djinn/skills.json` must not be
reintroduced. Settings are imported once into project-scoped Postgres before
normal coordinator dispatch begins.

### Tests

```sh
node --test scripts/test-djinn-retirement-manifest.mjs
```
