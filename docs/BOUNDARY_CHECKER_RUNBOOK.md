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

## Capability-boundary guards

In addition to the crate-level architectural checker above, the repository ships
**capability-boundary guards** that confine three external capabilities — git,
outbound HTTP, and Kubernetes — to dedicated *owner crates*. These guards are
pure-shell (POSIX `grep` over Rust sources), need no `cargo`, Postgres, or
network, and run in well under a second.

| Capability | Owner crate | Detector script |
|------------|-------------|-----------------|
| git (`git2::`, `use git2`, `Command::new("git")`, tokio `::new("git")`) | `server/crates/djinn-git` | `scripts/check-git-boundary.sh` |
| outbound HTTP (`reqwest::Client`, `ClientBuilder`, `RequestBuilder`, `reqwest::{`) | `server/crates/djinn-provider` | `scripts/check-http-boundary.sh` |
| Kubernetes (`kube::`, `use kube`, `k8s_openapi`, `Command::new("kubectl")`, tokio `::new("kubectl")`) | `server/crates/djinn-k8s` | `scripts/check-k8s-boundary.sh` |

Every other crate must go through the owner crate's API rather than declaring
the capability directly or shell-ing out to the relevant binary.

### Scripts and allowlist

- `scripts/check-capability-boundaries.sh` — shared guard plumbing. The
  per-capability scripts delegate to it by exporting `CAPABILITY`, `OWNER`,
  `REMEDIATION`, and a `PATTERN` (POSIX extended grep), then `exec`-ing it.
- `scripts/check-git-boundary.sh`, `scripts/check-http-boundary.sh`,
  `scripts/check-k8s-boundary.sh` — per-capability detector wrappers.
- `scripts/test-capability-boundaries.sh` — integrated self-test harness. It
  creates synthetic fixture files, exercises every matcher, the allowlist
  (exact hits, broad-glob rejection, missing-field rejection), empty input,
  file-list mode, full-tree equivalence, and a baseline inventory where all
  three detectors run clean against the live tree. Exits 0 on success and
  cleans up all fixtures via an `EXIT` trap.
- `scripts/capability-boundary-allowlist.toml` — auditable exemption list. Each
  `[[entries]]` block names a `capability`, exact `path`, exact `matcher`,
  `owner`, `rationale`, and at least one of `expires` (ISO date) or
  `cleanup_issue`. Broad globs are rejected (except narrow synthetic-fixture
  globs used by self-tests).

### CI wiring

The **`server-capability-boundaries`** job in
`.github/workflows/quality-gate.yml` (display name **`Server Capability
Boundaries`**) enforces all three guards. It mirrors the
`server-raw-sql-boundary` job exactly:

- Runs on `pull_request`, `merge_group`, and `workflow_dispatch` — **not** on
  push (which is cache-warming only).
- Gated by `needs.changes.outputs.server == 'true' && github.event_name != 'push'`.
- Checks out with `fetch-depth: 0` so `git diff` can resolve the merge base.
- **Runs `./scripts/test-capability-boundaries.sh` first**, *before* any live
  scan, so a detector or allowlist regression fails loudly instead of
  silently passing.
- Then runs the three live scans (`check-git-boundary.sh`,
  `check-http-boundary.sh`, `check-k8s-boundary.sh`) with `BASE_SHA` selected
  per event: `github.event.pull_request.base.sha ||
  github.event.merge_group.base_sha || ''`. An empty `BASE_SHA` makes the
  detectors fall back to `origin/main`, which is how `workflow_dispatch`
  produces a full-tree/baseline audit without fragile fetch behavior.

#### Enforcement via the aggregate `Quality Gate`

The capability-boundary job is **not** a separately required status check.
Like every other job in this workflow (`server-clippy`, `server-test`,
`server-raw-sql-boundary`, etc.), it is wired into the aggregate
**`Quality Gate`** job in *both* its `needs:` list and the `results=(…)`
array. `Quality Gate` is the single required status check enforced by branch
protection and the merge queue. It treats `skipped` as success (so a PR that
doesn't touch server files is not blocked by a skipped server job) but fails
on any `failure`/`cancelled`/`timed_out` result. Therefore individual server
jobs must never be listed as required on their own — they are skipped on
non-server PRs, and a skipped required check would block merges.

### Path-trigger coverage

The `changes` filter's `server:` list includes every guard artifact so that
**guard-only edits** still select the server gate:

- `scripts/test-capability-boundaries.sh`
- `scripts/check-capability-boundaries.sh`
- `scripts/check-git-boundary.sh`
- `scripts/check-http-boundary.sh`
- `scripts/check-k8s-boundary.sh`
- `scripts/capability-boundary-allowlist.toml`

This means an edit to *just* the allowlist, or *just* a detector script, or
*just* the self-test harness, will trigger the `server-capability-boundaries`
job — the self-test plus live scan will run on that PR, preventing a silent
detector regression from merging.

### Local commands

**Self-tests** (no cargo/python/network needed; run from the repo root):

```bash
sh scripts/test-capability-boundaries.sh
```

**Diff-mode scan** (the CI default; checks only changed Rust files between
`BASE_SHA` and `HEAD`):

```bash
# Defaults to origin/main when BASE_SHA is unset.
BASE_SHA=<base-sha> ./scripts/check-git-boundary.sh
BASE_SHA=<base-sha> ./scripts/check-http-boundary.sh
BASE_SHA=<base-sha> ./scripts/check-k8s-boundary.sh
```

**Full-tree / baseline scan** (pipe every Rust source through file-list mode;
this is what the self-test's baseline inventory and the `workflow_dispatch`
audit exercise):

```bash
find server -name '*.rs' | sh scripts/check-git-boundary.sh --files-from-stdin
find server -name '*.rs' | sh scripts/check-http-boundary.sh --files-from-stdin
find server -name '*.rs' | sh scripts/check-k8s-boundary.sh --files-from-stdin
```

**cargo-deny bans validation** (validates the direct-dependency wrapper
ratchets in `server/deny.toml`; see the ratchet-semantics section below):

```bash
cd server && cargo deny check bans
```

### cargo-deny wrapper-ratchet semantics

`server/deny.toml` carries a small set of `[bans]` *wrapper ratchets* under
`deny = [ … ]`. Each `{ crate = "<crate>", wrappers = [ … ] }` entry rejects any
**new** workspace crate that directly declares the dependency unless it is in
the `wrappers` allowlist. These ratchets are intentionally narrow and
**secondary** to the source capability-boundary scripts: they only stop new
*direct dependency declarations*; they do **not** prove that call sites are
confined to the owner crate, and they do not catch shell-outs (`Command::new`),
transitive helper crates, or in-cluster API calls through raw HTTP clients.

| Crate | Ratcheted? | Rationale |
|-------|-----------|-----------|
| `sqlx` | Yes | Only database-owner crates (`djinn-server`, `djinn-agent`, `djinn-control-plane`, `djinn-core`, `djinn-db`, `djinn-memory`, `djinn-provider`) may declare it directly. |
| `kube` | Yes | Only `djinn-k8s` and `djinn-image-controller` are first-party direct wrappers. |
| `k8s-openapi` | Yes | Same wrappers plus kube-internal transitive crates (`kube`, `kube-runtime`, `kube-core`, `kube-client`). |
| `git2` | **No** | After the capability-boundary migration, all first-party direct `git2` declarations are behind the owner crate `djinn-git` (and `vendored`-feature forwarders). A wrapper ratchet would have to list ~7 current direct dependents, providing no meaningful gate. New git usage should surface as a dependency on `djinn-git`, not on `git2`. The source script (`scripts/check-git-boundary.sh`) is the authority. |
| `reqwest` | **No** | The HTTP owner crate is `djinn-provider`, but five first-party crates still declare `reqwest` directly and several third-party crates pull it transitively. An allowlist that broad defeats the ratchet. Once the HTTP migration narrows direct usage to `djinn-provider`, a ratchet becomes useful; until then `scripts/check-http-boundary.sh` is the authority. |

**`workspace-hack` treatment.** The hakari-generated `workspace-hack` crate
mirrors the unified dependency graph and is not a normal consumer of any
capability. It is included in a ratchet's `wrappers` list **only where hakari
generates a direct dependency on the banned crate** (e.g. `sqlx`), and is
intentionally omitted where it does not (e.g. `k8s-openapi`). This is
documented inline in each entry's comments.

**Why source guards remain authoritative.** cargo-deny operates on the declared
dependency graph (manifest edges), so it catches direct dependency *spread* —
a useful early signal. But the real capability boundary is about *call-site
confinement*: which crate actually invokes `git2::`, shells out to `git`, or
constructs an `reqwest::Client`. cargo-deny cannot see shell-outs
(`Command::new("git")`), cannot confine call sites within an allowed-wrapper
crate, and does not inspect source at all. The pure-shell detector scripts
(`check-git-boundary.sh`, `check-http-boundary.sh`, `check-k8s-boundary.sh`)
and their allowlist are therefore the authority for shell-outs and call-site
confinement; the cargo-deny ratchets are a supplemental drift signal only.

## Related files

- `scripts/check_boundaries.py` — the architectural boundary checker (with `--self-test`)
- `server/boundary_rules.toml` — checked-in architectural rule set
- `Makefile` target `check-boundaries` — local entry point for the architectural checker
- `.github/workflows/quality-gate.yml` — `server-boundaries`, `server-raw-sql-boundary`, and `server-capability-boundaries` jobs + aggregate `quality-gate` wiring
- `scripts/check-capability-boundaries.sh` — shared capability-boundary guard plumbing
- `scripts/check-git-boundary.sh`, `scripts/check-http-boundary.sh`, `scripts/check-k8s-boundary.sh` — per-capability detectors
- `scripts/test-capability-boundaries.sh` — integrated self-test harness
- `scripts/capability-boundary-allowlist.toml` — capability exemption allowlist
- `server/deny.toml` — cargo-deny advisories/licenses/bans (wrapper ratchets for capability-owner crates)
