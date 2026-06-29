# Architectural Boundary Checker — Runbook

This documents the repo-local architectural boundary checker
(`scripts/check_boundaries.py`) and how it is wired into CI.

It is **lightweight by design**: it reads crate manifests and greps source. It
does **not** invoke `cargo`, touch a database, warm a SCIP/code graph, or
compile the workspace. A full run finishes in well under a second, which is why
it is safe to wire as a hard PR / merge-queue gate.

> History: an earlier iteration loaded a *warmed canonical graph* from Postgres
> (`repo_graph_cache`) and required a `warm-graph` step that ran the SCIP
> indexers (rust-analyzer etc.) on every run — minutes of CI time plus a
> Postgres service. That approach was dropped in favour of this script because
> the enforced rules are all expressible from declared crate dependencies plus a
> tiny file-level import scan, with no graph needed.

## What it checks

The checker loads `server/boundary_rules.toml` and enforces its forbidden-edge
rules. Rules come in two flavours:

- **Crate-level** (`from_glob` reduces to a bare crate-name pattern, e.g.
  `**/djinn-agent/**` → `djinn-agent`): matched against the inter-crate
  dependency edges declared in each crate's `Cargo.toml` (`[dependencies]` and
  `[build-dependencies]`). `dev-dependencies` are **excluded** — test-only deps
  are not a production-layering violation.
- **File-level** (`from_glob` keeps a path shape, e.g.
  `**/actors/slot/pool/{actor,handle,mod,types}.rs`): the matching source files
  are scanned for a `use`-style reference to the forbidden crate's module token
  (`djinn-k8s` → `djinn_k8s`).

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0    | Clean — no boundary violations |
| 1    | One or more violations found (human-readable report on stderr) |
| 2    | Operational error (unreadable/invalid rules file, no crate manifests found) |

## Running it

```bash
make check-boundaries          # → python3 scripts/check_boundaries.py
# or directly:
python3 scripts/check_boundaries.py
python3 scripts/check_boundaries.py --rules server/boundary_rules.toml
python3 scripts/check_boundaries.py --self-test   # internal logic tests
```

Requires **Python 3.11+** (uses the stdlib `tomllib`); no other dependencies.

A clean run prints, e.g.:

```
✓ No boundary violations found. (checked 7 rule(s) — 6 crate-level — against 146 declared crate edge(s) across 27 crates)
```

A violation prints a report naming the rule and the witness edge, and exits 1:

```
✗ 1 boundary violation(s) found:

  server/crates/djinn-agent/src/actors/slot/pool/types.rs → djinn-k8s
      rule name:   slot-pool-no-k8s-direct-import
      description: Production slot pool lifecycle code must not import djinn_k8s directly; use the RuntimeOps bridge layer.
      from_key:    server/crates/djinn-agent/src/actors/slot/pool/types.rs
      to_key:      djinn-k8s
      witness:     server/crates/djinn-agent/src/actors/slot/pool/types.rs references `djinn_k8s`
```

### Forbidden-edge demonstration (no real violation committed)

```bash
# Inject, observe the failure, then restore:
f=server/crates/djinn-agent/src/actors/slot/pool/types.rs
printf '\nuse djinn_k8s as _demo;\n' >> "$f"
python3 scripts/check_boundaries.py   # → exit 1, names slot-pool-no-k8s-direct-import
git checkout -- "$f"
```

## Rule file

Every `[[rules]]` entry in `server/boundary_rules.toml` is validated at startup:
nonblank `name` / `from_glob` / `to_glob`, and a meaningful (non-boilerplate)
`description`. Any failure exits 2 with a per-rule message.

### Enforced rules (baseline green on the current tree)

| Rule | Kind | Invariant |
|------|------|-----------|
| `no-agent-imports-graph` | crate | djinn-agent must not depend on djinn-graph |
| `slot-pool-no-k8s-direct-import` | file | slot-pool lifecycle files must not import `djinn_k8s` |
| `leaf-core-imports-no-djinn-crate` | crate | djinn-core depends on no other djinn crate |
| `leaf-stack-imports-no-djinn-crate` | crate | djinn-stack depends on no other djinn crate |
| `leaf-telemetry-imports-no-djinn-crate` | crate | djinn-telemetry depends on no other djinn crate |
| `no-roles-imports-agent` | crate | djinn-roles must not depend on djinn-agent |
| `no-roles-imports-extension` | crate | djinn-roles must not depend on djinn-mcp-extension |

### Deferred (aspirational) rules — intentionally NOT enforced

Three target-architecture rules are documented but commented out in the rules
file because the current tree heavily violates them, so they cannot be enforced
as hard "zero" rules without a false-green baseline:

| Rule | Real violations today |
|------|-----------------------|
| `no-agent-imports-control-plane` | ~20 files reference `djinn_control_plane` |
| `no-agent-imports-db` | ~44 files reference `djinn_db` |
| `no-db-imports-memory` | ~15 files reference `djinn_memory` |

These describe routing the agent's storage/orchestration access through a bridge
instead of direct crate deps — a real refactor, not a CI-wiring task. To guard
against *new* occurrences before that refactor lands, add a baseline-allowlist
("no new violations") mode to `scripts/check_boundaries.py` and re-introduce the
rules.

## CI wiring

The `server-boundaries` job in `.github/workflows/quality-gate.yml` runs
`python3 scripts/check_boundaries.py` on `pull_request`, `merge_group`, and
`workflow_dispatch` when server-relevant files change. It needs no Postgres, no
build step, and no `server-clippy` dependency — it mirrors the existing
`server-raw-sql-boundary` script guard. It is a member of the aggregate
`quality-gate` job, so a violation blocks the merge.

Path-trigger coverage: the `changes` filter includes `server/boundary_rules.toml`
(rule-only edits), `scripts/check_boundaries.py` (checker-only edits), and
`.github/workflows/quality-gate.yml` (workflow-only edits), so each of those
exercises the gate.

## Related files

- `scripts/check_boundaries.py` — the checker (with `--self-test`)
- `server/boundary_rules.toml` — checked-in rule set
- `Makefile` target `check-boundaries` — local entry point
- `.github/workflows/quality-gate.yml` — `server-boundaries` job + gate wiring
