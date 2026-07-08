# Pre-task lifecycle hooks (`lifecycle.pre_task`)

Pre-task commands let each project declare shell commands that run inside the
task-run Pod **before** the agent supervisor starts.  The canonical use case is
test-database preparation — running migrations, seeding fixtures, or applying
schema files against a Postgres sidecar — but any deterministic setup step works.

- [Quick example](#quick-example)
- [Command shape and YAML format](#command-shape-and-yaml-format)
- [Framework examples](#framework-examples)
  - [Rails (ActiveRecord)](#rails-activerecord)
  - [Django](#django)
  - [Prisma](#prisma)
  - [Raw SQL / psql](#raw-sql--psql)
  - [Generic shell](#generic-shell)
- [Validation constraints](#validation-constraints)
- [Failure policies](#failure-policies)
- [Timeout and cancellation](#timeout-and-cancellation)
- [Output redaction and truncation](#output-redaction-and-truncation)
- [Activity events (`task_run_pretask_ran`)](#activity-events-task_run_pretask_ran)
- [Injected connection environment variables](#injected-connection-environment-variables)
- [Rollout sequencing](#rollout-sequencing)
- [Djinn's own `djinn_test_template` exception](#djinns-own-djinn_test_template-exception)

---

## Quick example

```yaml
schema_version: 1
lifecycle:
  pre_task:
    - name: migrate-test-db
      command: psql "$TEST_POSTGRES_URL" -f schema.sql
      timeout_seconds: 120
      failure_policy: blocking
```

This runs `psql "$TEST_POSTGRES_URL" -f schema.sql` at the project root in the
task-run Pod.  If the command exits non-zero, the task run is classified as an
environmental non-attempt (not a code failure).

## Command shape and YAML format

Each entry in `lifecycle.pre_task` is a `PreTaskCommand`:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | `pre_task_N` (1-based) | Display/identity name. When omitted, auto-generated as `pre_task_1`, `pre_task_2`, etc. |
| `command` | string | **required** | Shell string passed to `/bin/sh -c`. The process inherits the worker environment, including injected service connection env vars. |
| `timeout_seconds` | integer | `300` (5 min) | Wall-clock timeout per command. |
| `failure_policy` | string | `blocking` | What to do on non-zero exit: `blocking` or `best_effort`. |

Commands execute **sequentially** in array order, at the project root directory,
with the full worker environment (including k8s-injected service connection
strings).

## Framework examples

### Rails (ActiveRecord)

Prepare a test database by running Rails migrations against an injected
Postgres sidecar:

```yaml
schema_version: 1
lifecycle:
  pre_task:
    - name: rails-db-prepare
      command: |
        export DATABASE_URL="$TEST_POSTGRES_URL"
        bundle exec rails db:create db:schema:load
      timeout_seconds: 300
      failure_policy: blocking
```

### Django

Run Django migrations against the injected connection:

```yaml
schema_version: 1
lifecycle:
  pre_task:
    - name: django-migrate
      command: |
        export DATABASE_URL="$TEST_POSTGRES_URL"
        python manage.py migrate
      timeout_seconds: 300
      failure_policy: blocking
    - name: django-seed
      command: python manage.py loaddata test_fixtures
      timeout_seconds: 120
      failure_policy: best_effort
```

### Prisma

Deploy Prisma migrations against the injected connection:

```yaml
schema_version: 1
lifecycle:
  pre_task:
    - name: prisma-deploy
      command: |
        export DATABASE_URL="$TEST_POSTGRES_URL"
        npx prisma migrate deploy
      timeout_seconds: 300
      failure_policy: blocking
```

### Raw SQL / psql

Apply a plain SQL schema file:

```yaml
schema_version: 1
lifecycle:
  pre_task:
    - name: apply-schema
      command: psql "$TEST_POSTGRES_URL" -f schema.sql
      timeout_seconds: 120
      failure_policy: blocking
```

or a multi-step sequence:

```yaml
schema_version: 1
lifecycle:
  pre_task:
    - name: create-db
      command: psql "$TEST_POSTGRES_URL" -c "CREATE DATABASE test_db"
      timeout_seconds: 30
      failure_policy: best_effort
    - name: apply-schema
      command: psql "$TEST_POSTGRES_URL" -f migrations/001_init.sql
      timeout_seconds: 120
      failure_policy: blocking
    - name: seed-fixtures
      command: psql "$TEST_POSTGRES_URL" -f seeds/test_data.sql
      timeout_seconds: 60
      failure_policy: best_effort
```

### Generic shell

Any deterministic setup step:

```yaml
schema_version: 1
lifecycle:
  pre_task:
    - name: install-deps
      command: pip install -e ".[test]"
      timeout_seconds: 300
      failure_policy: blocking
    - name: compile-assets
      command: npm run build
      timeout_seconds: 180
      failure_policy: blocking
    - name: warm-cache
      command: python scripts/warm_cache.py
      timeout_seconds: 60
      failure_policy: best_effort
```

## Validation constraints

The control plane validates `lifecycle.pre_task` on every config set/update.
Invalid configs are rejected before they reach the database:

| Constraint | Value | Error |
|------------|-------|-------|
| **Empty command** | `command` must be non-empty | `EmptyValue` |
| **Command length** | `command` ≤ 4,096 bytes | `TooLong` |
| **Timeout range** | `timeout_seconds` ∈ [1, 1800] (1 second to 30 minutes) | `OutOfRange` |
| **Timeout default** | `timeout_seconds` defaults to 300 (5 min) when omitted | — |
| **List size** | ≤ 20 commands in `lifecycle.pre_task` | `ListTooLong` |
| **Name uniqueness** | Resolved names (supplied or auto-generated) must be unique across the list | `DuplicateName` |
| **Name identifier** | When supplied, `name` must be a valid identifier (alphanumeric, hyphens, underscores) | `InvalidIdentifier` |

## Failure policies

Each command carries a `failure_policy` that determines what happens when it
exits non-zero, times out, or is cancelled:

| Policy | Behavior |
|--------|----------|
| `blocking` (default) | The task run is **blocked**.  No further pre-task commands execute.  The failure is classified as an **environmental non-attempt** (`pre_task_failed`, `pre_task_timed_out`, or `pre_task_cancelled`), not a code failure.  This means the run does not count as an agent mistake — it reflects an infrastructure/setup problem. |
| `best_effort` | The failure is **logged** and execution continues with the next command.  If any best-effort commands failed, the overall result is `BestEffortFailure`, but the supervisor starts normally. |

When a blocking command fails, the runner returns a `PreTaskCommandsResult::Blocked`
variant carrying the blocking command's result.  The worker classifies the failure:

| Condition | Classification string |
|-----------|----------------------|
| Blocking command exit ≠ 0 | `pre_task_failed` |
| Blocking command timed out | `pre_task_timed_out` |
| Blocking command cancelled (pod-level) | `pre_task_cancelled` |

## Timeout and cancellation

Each command is spawned as `/bin/sh -c <command>` in its own **process group**
at the project root.  Three termination paths are supported:

1. **Normal exit** — the command finishes within `timeout_seconds`.  Exit code
   is captured and reported.
2. **Timeout** — if the command exceeds `timeout_seconds`, the runner sends
   **SIGTERM** to the entire process group, waits 5 seconds for a graceful
   shutdown, then escalates to **SIGKILL**.
3. **Pod-level cancellation** — if the pod's cancellation token fires (e.g.
   soft deadline reached), the runner similarly terminates the process group
   with grace-then-kill.

A synthetic cancelled result is emitted for any command that was skipped because
a prior cancellation or blocking failure stopped the sequence mid-list.

## Output redaction and truncation

Combined stdout + stderr from each command is:

1. **Redacted** — all values of environment variables whose names match common
   secret patterns (`SECRET`, `TOKEN`, `PASSWORD`, `API_KEY`, `PRIVATE_KEY`,
   `ACCESS_KEY`, `CREDENTIAL`, `AUTH`) and all k8s-injected service connection
   strings are replaced with `[REDACTED]` in the captured output and in the
   `command` field of activity payloads.  Values ≤ 4 characters are skipped to
   avoid over-redacting common substrings.
2. **Truncated** — only the final **16 KiB** (16,384 bytes) of combined output
   is retained.  When truncation occurs, a `--- output truncated ---` marker is
   prepended to the retained tail.

The `output_truncated` boolean field in both the result struct and the activity
payload indicates whether truncation occurred.

## Activity events (`task_run_pretask_ran`)

Exactly **one** `task_run_pretask_ran` activity event is emitted per started
command, including synthetic entries for cancelled-and-skipped commands.  Events
are best-effort: a sink failure is logged but never blocks the worker from
continuing.

### Payload shape

Each `task_run_pretask_ran` event carries a JSON payload with these stable fields:

```json
{
  "name": "migrate-test-db",
  "index": 0,
  "command": "psql \"$TEST_POSTGRES_URL\" -f schema.sql",
  "failure_policy": "blocking",
  "started_at": "2026-07-08T12:34:56.789Z",
  "duration_ms": 4521,
  "exit_code": 0,
  "timed_out": false,
  "cancelled": false,
  "blocked": false,
  "output_tail": "...",
  "output_truncated": false
}
```

When a **blocking** command fails, times out, or is cancelled, two extra fields
appear:

```json
{
  "blocked": true,
  "failure_class": "environmental"
}
```

The `command` and `output_tail` fields are redacted before emission (see
[Output redaction and truncation](#output-redaction-and-truncation)).

### Additive compatibility

`task_run_pretask_ran` is an **additive** event type.  Activity consumers that
do not recognize this event name simply ignore it.  Existing consumers (the
activity log UI, SSE listeners) treat unknown event types as opaque entries and
display them without error.  Rolling deployments where only some worker pods
emit this event are safe: the host database `activity_log` table is schemaless
for the `payload` column, and the event type string is stable.

## Injected connection environment variables

Pre-task commands inherit the worker Pod's environment, which includes:

- **Service connection strings** injected by the k8s sidecar mechanism.  For a
  Postgres service preset, the connection string is exported as the env var
  named in the preset's `conn_env_var` field (e.g. `TEST_POSTGRES_URL`).
  The exact name is visible in the task-run's `service_metadata.json` mount.
- **Environment config env vars** from the project's `EnvironmentConfig.env`
  map.
- **Standard worker env vars** (`HOME`, `PATH`, workspace paths, etc.).

The injected env vars are the intended mechanism for pre-task commands to
locate backing services.  Commands should reference them directly (e.g.
`psql "$TEST_POSTGRES_URL"`), not hardcode hostnames or ports.

## Rollout sequencing

The `lifecycle.pre_task` feature was delivered across multiple epics and must
be rolled out in this order:

### Phase 1: Schema readers / no-op defaults (already shipped)

The `lifecycle.pre_task` field was added to `EnvironmentConfig` with a serde
default of `[]` (empty list).  Existing projects that have no `pre_task`
configuration continue to work unchanged — the worker sees an empty list and
skips pre-task execution entirely.

### Phase 2: Effective config mount + worker execution support (already shipped)

The hgd0 epic shipped Secret-backed mounting of the effective
`EnvironmentConfig` into task-run pods at
`/var/run/djinn/environment.json`.  The worker's lifecycle runner reads this
mount, executes `pre_task` commands sequentially, and emits
`task_run_pretask_ran` activity events.  Environmental non-attempt
classification routes blocking failures as infrastructure problems.

### Phase 3: Config validation on write (already shipped)

The control plane validates `lifecycle.pre_task` on every config set/update
(rejecting empty commands, out-of-range timeouts, duplicate names, and
oversized lists).  Image and project configs round-trip the field through the
JSONB column.

### Phase 4: Non-empty project/image configs (current)

After phases 1-3 are live, projects and images **may** declare non-empty
`lifecycle.pre_task` entries.  Operators should:

1. Confirm the control plane and worker images include phases 1-3 code.
2. Ensure service presets (e.g. `postgres:16`) are declared on images that need
   backing services.
3. Add `lifecycle.pre_task` entries to the project or image environment config.
4. Verify `task_run_pretask_ran` events appear in the activity log.

### Phase 5: Activity consumers treat `task_run_pretask_ran` as additive

Downstream activity consumers (UI, SSE, analytics) must treat
`task_run_pretask_ran` as an additive event type.  Unrecognized event types
should be displayed opaquely and must not cause errors.  This is already the
default behavior for the activity log's schemaless payload column.

**No unverifiable external deployment proof is required.**  The rollout rule is
documented here as release/operator sequencing — the in-repo tests verify the
contract, and the CI gate confirms it.

## Djinn's own `djinn_test_template` exception

Djinn's own repo uses an **intentional in-process exception** for test-database
preparation that predates the generic `lifecycle.pre_task` mechanism:

**`server/crates/djinn-db/src/template_bootstrap.rs`** bootstraps
`djinn_test_template` by:

1. Acquiring a local per-process semaphore and a Postgres exclusive advisory
   lock to serialize concurrent creation across pods.
2. Creating the `djinn_test_template` database (if absent) and marking it as a
   Postgres template.
3. Running djinn's compiled `sqlx::migrate!()` migrations against it.

This helper is hardcoded to djinn's schema name, migration path, and advisory
lock id.  It is **not** the generic path for target repos and must not be
generalized in place.

### Why not use `lifecycle.pre_task`?

The `pre_task` runner executes shell commands via `/bin/sh -c`.  Djinn's
template bootstrap needs:

- **Postgres advisory locking** (`pg_advisory_lock` / `pg_advisory_unlock`)
  across concurrent pods.
- **Compiled sqlx migrations** via `sqlx::migrate!()`, which embeds migration
  SQL at compile time.

Neither is available through a shell command today.  A future migration could
move this into `lifecycle.pre_task` if a generic advisory-lock shell wrapper
and compiled-migration binary are shipped.

### The generic equivalent for any other repo

For any target repo that is **not** djinn itself, the intended path is:

```yaml
lifecycle:
  pre_task:
    - name: prepare-test-db
      command: <framework migration command consuming $TEST_POSTGRES_URL>
      timeout_seconds: 300
      failure_policy: blocking
```

The Postgres sidecar is stood up by the service preset mechanism, and the
connection string is injected as an environment variable.  The pre-task command
runs the framework's migration tool against that connection.  No djinn-core
code changes are needed for the target repo.
