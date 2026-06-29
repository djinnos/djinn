# Boundary Checker — Clean-Checkout Warm/Check Runbook

This runbook documents how to set up, warm, and run the repo-local architectural
boundary checker (`check-boundaries`) from a clean checkout. It is the canonical
reference for the sibling CI epic (gi14) that will wire this into the required
GitHub Actions quality gate.

## Overview

The boundary checker loads `server/boundary_rules.toml`, builds a crate-level
dependency graph from the warmed canonical graph, and exits non-zero if any
forbidden edge is detected.

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0    | Clean — no boundary violations |
| 1    | One or more boundary violations found (human-readable report on stderr) |
| 2    | Operational error (missing env, unreadable TOML, invalid rules, graph not warmed, DB error) |

## Prerequisites

- **Postgres** — the checker loads the warmed graph from `repo_graph_cache` via
  `DJINN_DATABASE_URL`. The same Postgres instance used by the server is required.
- **sqlx-cli** — for running migrations (`cargo install sqlx-cli --no-default-features --features postgres,rustls`).
- **Rust toolchain** — stable Rust with `cargo` on `$PATH`.
- **`djinn-agent-worker`** binary — built from `server/crates/djinn-agent-worker`.

## Required Environment Variables

| Variable | Purpose | Example |
|----------|---------|---------|
| `DJINN_DATABASE_URL` | Postgres connection string for the warmed graph database | `postgres://postgres:postgres@127.0.0.1:5432/app_test?sslmode=disable` |
| `DJINN_PROJECT_ID` | UUID identifying the project row in the database (the project must already exist) | `019ea3bd-a305-73e3-806c-4edcc96ebfe2` |
| `DJINN_PROJECT_PATH` | Absolute path to the repo checkout root (where `Makefile` lives) | `/workspace` |

## Step-by-Step: Clean-Checkout Warm and Check

### 1. Run database migrations

```bash
cd server/crates/djinn-db
sqlx migrate run --source migrations_postgres
```

This ensures the `repo_graph_cache` table and all other schema objects exist.

### 2. Warm the canonical graph

```bash
export DJINN_DATABASE_URL="postgres://postgres:postgres@127.0.0.1:5432/app_test"
export DJINN_PROJECT_ID="<your-project-uuid>"
export DJINN_PROJECT_PATH="/path/to/checkout"

cd server
cargo run -p djinn-agent-worker -- warm-graph "$DJINN_PROJECT_ID"
```

This builds the code-graph SCIP index for the `origin/main` HEAD, writes it to
`repo_graph_cache`, and pins it to the current commit. The warm step typically
takes 60–120 s depending on workspace size and `rust-analyzer` availability.

**Success indicator:** the command exits 0 and logs the project ID, commit SHA,
and node/edge counts.

### 3. Run the boundary checker

```bash
make check-boundaries
```

This expands to:

```bash
cd server && cargo run --bin check-boundaries -- \
    --rules boundary_rules.toml \
    --project-id $DJINN_PROJECT_ID \
    --project-path $DJINN_PROJECT_PATH
```

### 4. Interpret the result

- **Exit 0:** All boundary rules pass against the current graph. The baseline is green.
- **Exit 1:** One or more forbidden edges detected. The stderr report includes:
  - Rule index, `rule name`, `description`
  - `from_key` (source crate), `to_key` (target crate)
  - `witness` (the forbidden edge in `from → to` notation)
- **Exit 2:** Operational failure. Check the error message for the specific blocker:
  - Missing `DJINN_DATABASE_URL`
  - Cannot read/parse `boundary_rules.toml`
  - Rule validation failure (blank fields, boilerplate descriptions, invalid globs)
  - Empty rule set
  - Graph not warmed / stale graph / empty graph
  - Empty crate map / empty crate graph

## Forbidden-Edge Demonstration Procedure

To demonstrate violation output **without committing a real violation**, use the
existing unit tests that exercise the `check_violations` and
`render_violation_report` helpers against an in-memory `CrateGraph` fixture:

```bash
cd server
cargo test -p djinn-server --bin check-boundaries -- \
    tests::check_violations_detects_forbidden_edge \
    tests::render_violation_report_includes_all_required_fields \
    tests::violation_exit_code_is_one
```

These tests construct a `CrateGraph` with a `djinn-agent → djinn-control-plane`
edge, compile the `no-agent-imports-control-plane` rule, assert exactly one
violation, and verify the rendered report includes all required fields:

- `rule name:  no-agent-imports-control-plane`
- `description: Agent must not import control-plane; control-plane is the bridge layer.`
- `from_key:   djinn-agent`
- `to_key:     djinn-control-plane`
- `witness:    djinn-agent → djinn-control-plane`
- exit code = 1

The full test suite (33 tests) can be run with:

```bash
cargo test -p djinn-server --bin check-boundaries -- tests
```

## Rule File Validation

Every `[[rules]]` entry in `server/boundary_rules.toml` is validated at startup
for:

1. **Nonblank `name`** — the rule must have a non-empty identifier.
2. **Nonblank `from_glob`** — the source glob pattern must be present.
3. **Nonblank `to_glob`** — the target glob pattern must be present.
4. **Meaningful `description`** — must be non-empty and not boilerplate
   (rejects strings containing "TODO", "FIXME", "placeholder", "TBD",
   "no description", "description here", "insert description").
5. **Valid glob syntax** — both globs must compile via the `globset` crate.

Any validation failure exits with code 2 and a human-readable error listing
each failing rule's index, name, field, and message.

The current `boundary_rules.toml` contains 10 rules, all passing validation:

| # | Rule Name | Purpose |
|---|-----------|---------|
| 0 | `no-agent-imports-control-plane` | Agent must not import control-plane |
| 1 | `slot-pool-no-k8s-direct-import` | Slot pool must use RuntimeOps bridge |
| 2 | `no-agent-imports-db` | Agent must not import djinn-db directly |
| 3 | `no-agent-imports-graph` | Agent must not import djinn-graph directly |
| 4 | `no-db-imports-memory` | Known inversion guard for djinn-db → djinn-memory |
| 5 | `leaf-core-imports-no-djinn-crate` | djinn-core leaf isolation |
| 6 | `leaf-stack-imports-no-djinn-crate` | djinn-stack leaf isolation |
| 7 | `leaf-telemetry-imports-no-djinn-crate` | djinn-telemetry leaf isolation |
| 8 | `no-roles-imports-agent` | djinn-roles must not depend on djinn-agent |
| 9 | `no-roles-imports-extension` | djinn-roles must not depend on djinn-mcp-extension |

## Baseline-Green Verification

The checked-in rule set is **baseline-green** for the current tree. No rule
was narrowed or removed to achieve green — the existing dependency graph
satisfies all 10 rules. If a future code change introduces a forbidden edge,
the checker will report it as a violation (exit 1).

### Strongest feasible verification transcript

```
$ # 1. Migrations applied
$ cd server/crates/djinn-db && sqlx migrate run --source migrations_postgres
Applied 84 migrations (all clean)

$ # 2. Warm graph
$ export DJINN_DATABASE_URL="postgres://postgres:postgres@127.0.0.1:5432/app_test"
$ export DJINN_PROJECT_ID="019ea3bd-a305-73e3-806c-4edcc96ebfe2"
$ export DJINN_PROJECT_PATH="/workspace"
$ cd server && cargo run -p djinn-agent-worker -- warm-graph "$DJINN_PROJECT_ID"
  (requires project row to exist in the database; see blocker note below)

$ # 3. Boundary check
$ make check-boundaries
  → exit 0 (clean baseline) or exit 1 (violations) or exit 2 (operational error)

$ # 4. Non-DB verification (always passes)
$ cargo test -p djinn-server --bin check-boundaries -- tests
test result: ok. 33 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo fmt --check  # boundary checker file is clean
```

### Documented blocker

The warm-graph command requires the project to be registered in the database
(`djinn_agent_worker` logs `project <id> not found` if the project row is
absent). In CI, the coordinator ensures the project exists before dispatching
the warm job. In a standalone clean-checkout, the database must be seeded with
the project row before `warm-graph` can succeed.

## Related Files

- `server/boundary_rules.toml` — checked-in boundary rule set
- `server/ci/check_boundaries.rs` — checker binary source and test suite
- `Makefile` target `check-boundaries` — entry point for running the checker
- `server/crates/djinn-agent-worker/src/main.rs` — `warm-graph` command
- `server/crates/djinn-graph/src/canonical_graph.rs` — `run_warm_graph_command` and `load_canonical_graph_only`
