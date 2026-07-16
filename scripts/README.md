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

## Rust size guard

Lightweight guard for Rust source files under `server/crates/**` and `server/src/**`. A file fails when it exceeds either size threshold.

### CI gate

`.github/workflows/quality-gate.yml` runs the `server-size-guard` job for PR and merge-queue server changes. The job computes added, modified, and renamed files with `git diff --name-only --diff-filter=AMR` and pipes that list to changed-file mode:

```sh
./scripts/check-file-size.sh --files-from-stdin
```

CI is a regression guard for new or edited Rust files; it does not full-tree scan every legacy file on each PR.

### Run locally

Changed-file mode, matching CI input style:

```sh
printf '%s\n' server/crates/foo/src/lib.rs | ./scripts/check-file-size.sh --files-from-stdin
```

Full-tree audit mode:

```sh
./scripts/check-file-size.sh --all
```

A full-tree audit may still report legacy oversized files until future split work lands.

### Thresholds

Defaults are `MAX_LINES=1500` and `MAX_BYTES=51200`; exceeding either limit fails the guard. Override either value with environment variables:

```sh
MAX_LINES=1200 MAX_BYTES=45000 ./scripts/check-file-size.sh --all
```

### Escape hatch

Add `// djinn:allow-oversize` anywhere in a file to allow an intentional exception. Use this only when a Rust source file genuinely needs to exceed the guideline and should not block CI.

### Skipped paths

Generated Rust files are skipped defensively: paths matching `**/generated/**` and files matching `*.gen.*`.

### Tests

```sh
sh scripts/test-check-file-size.sh
```

## Phase 1 retirement manifest generator

`djinn-retirement-manifest.mjs` is the hermetic retirement manifest generator
for Phase 1 knowledge retirement (epic h1w2 / proposal qiy6). It consumes
NUL-delimited `git ls-files -z` output plus explicit hermetic DB-selection and
DB-guidance fixture input and writes two deterministic JSON manifests under
`target/djinn-retirement/`:

- `knowledge-manifest.json` — one entry per tracked `.djinn` knowledge file,
  carrying repository path, committed blob SHA-256, normalized-content SHA-256,
  detected permalink, selected DB identity (UUID/permalink/status/normalized
  hash), and exactly one disposition (`equivalent`, `db_supersedes_file`, or
  `approved_discard`).
- `db-guidance-manifest.json` — one entry per affected DB guidance record,
  carrying selected identity, classification, disposition, rationale, status,
  hashes, and supersession linkage fields for the follow-up DB reconciliation
  task.

Normalization only removes YAML front matter and canonicalizes CRLF/CR to LF;
committed blob SHA-256 is computed over the exact stored bytes (`git show
HEAD:<path>`).

### Hermetic fixtures

Committed under `scripts/fixtures/djinn-retirement/`:

- `db-selection.json` — synthetic DB-selection records keyed by detected
  permalink (uuids are deterministic sha256 derivatives, not real DB uuids).
- `db-guidance.json` — synthetic DB-guidance records with classification and
  supersession linkage fields.

Regenerate from the current HEAD:

```sh
node scripts/fixtures/djinn-retirement/generate.mjs
```

### Strict reconciliation guard

```sh
make check-retirement-manifest
# or directly:
./scripts/check-djinn-retirement-manifest.sh
```

The guard runs the generator against live HEAD with the committed fixtures and
enforces strict invariants: ambiguous DB matches, duplicate paths,
tracked/deletion count or set mismatch, missing preserved identity, empty
discard reason or approving task id, missing guidance disposition, and
unresolved entries are all hard failures.

### Tests

```sh
node --test scripts/test-djinn-retirement-manifest.mjs
```
