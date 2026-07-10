# Development

The Rust workspace lives in `server/` (binary `djinn-server` plus ~16 crates
under `server/crates/`); the web client is in `ui/` (React + Vite +
TypeScript, pnpm). The UI is compiled into the server binary via `rust-embed`,
and the TypeScript MCP types are generated from the server's live tool schemas
(`pnpm --dir ui mcp:types`).

## Local stack (Tilt + kind)

The full stack runs in a local [kind](https://kind.sigs.k8s.io) cluster,
orchestrated by [Tilt](https://tilt.dev). One command brings up the cluster,
registry, server, Postgres, Qdrant, the image pipeline, and a self-hosted
Langfuse for tracing — built from your working tree.

**Prerequisites:** Docker, [kind](https://kind.sigs.k8s.io), `kubectl`,
[Helm](https://helm.sh), [Tilt](https://tilt.dev), [pnpm](https://pnpm.io)
(Node **≥ 22.13** — pnpm 11 needs `node:sqlite`; older Nodes die mid-install
with `ERR_UNKNOWN_BUILTIN_MODULE`), and `openssl`.

**Linux only:** the image pipeline runs BuildKit rootless via user
namespaces, which needs host sysctls (kind inherits them). On macOS, Docker
Desktop's VM already provides this — skip these:

```sh
sudo sysctl -w kernel.unprivileged_userns_clone=1
sudo sysctl -w user.max_user_namespaces=28633   # or higher
```

Then, from the repo root:

```bash
tilt up
```

> **First run with a non-kind kubectl context active** (e.g. `staging`):
> Tilt's production-context guard fires at Tiltfile parse time, *before* the
> bootstrap gets a chance to create the kind cluster and switch contexts, so
> `tilt up` aborts with "Refusing to run 'local'…". Bootstrap once by hand —
> `bash scripts/kind/setup-kind.sh` — which creates the cluster and switches
> the current context to `kind-djinn`; then `tilt up` works. (Switch back
> later with `kubectl config use-context <your-context>`.)

Tilt bootstraps the kind cluster (`djinn`) + a local registry, builds
`djinn-server` and `djinn-agent-worker`, embeds the freshly built UI, installs
the Helm chart, and port-forwards:

| Port | Service |
|------|---------|
| `:3000` | djinn API + web UI |
| `:8443` | worker RPC |
| `:5432` | Postgres |
| `:6333` / `:6334` | Qdrant (HTTP / gRPC) |
| `:5000` | Langfuse dashboard |
| `:9091` | MinIO console |

Open the UI at **http://127.0.0.1:3000**. `tilt down` removes the Helm release
but leaves the cluster up; `kind delete cluster --name djinn` tears it down
completely.

> The heavy build steps (`djinn-binaries`, `djinn-ui-dist`, runtime base
> image) are **manual** triggers in the Tilt UI — hit refresh on
> `djinn-binaries` to recompile after Rust changes; the server image and pod
> roll follow automatically.

## Worker GateGuard (edit + shell)

The worker role runs under a **code-enforced** pre-edit and pre-shell gate
called GateGuard. GateGuard is wired into the handler layer, not into prompt
prose — it fires regardless of what the model claims to have done — and it
is **worker-only**. Reviewer, planner, architect, and missing-role sessions
pass through unconditionally (existing role-independent behavior, such as
the reviewer/planner cargo steering, is unchanged).

The implementation lives in
`server/crates/djinn-agent/src/extension/handlers/gate_guard/` and is
exercised end-to-end by `server/crates/djinn-agent/src/extension/tests/`
(`edit_dispatch_tests.rs`, `shell_dispatch_tests.rs`,
`gate_guard_dispatch_tests.rs`). The classifier that decides what counts
as destructive shell behavior lives in
`server/crates/djinn-mcp-extension/src/command_classifier.rs`.

### Edit surfaces: investigation-before-edit

GateGuard code-enforces **investigation-before-edit** on every
worker edit/write/apply_patch surface (`call_edit`, `call_write`,
`call_apply_patch` in
`server/crates/djinn-agent/src/extension/handlers/workspace.rs`). The
dispatch order is:

1. `file_time.assert(...)` runs first and rejects the call if the file was
   never read in the current live session (you must read a file before
   editing it).
2. `gate_guard_edit_check` runs after `assert` succeeds and after the
   successful match byte range is known, but **before** the new content is
   written.
3. Inside `gate_guard_edit_check`:
   - If the latest read was **truncated** (`record.truncated == true`),
     the edit is denied with `FORCE-TRUNCATED-READ`; the diagnostic is
     recorded but `edit_forced` is **not** marked. The worker must
     re-read the entire file.
   - If the latest read does not **cover the byte range** being mutated
     (`!record.covers_span(span_start, span_end)`), the edit is denied
     with `FORCE-UNCOVERED-READ`. The worker must re-read with full
     coverage.
   - On the **first** covered, non-truncated edit per (path,
     live-session), the worker is shown an investigation FORCE prompt
     demanding importers/callers, affected public API, data shapes, and
     the verbatim task instruction. The path is then marked in
     `edit_forced` so subsequent edits to the same path in the same
     session proceed without re-prompting.
   - After `edit_forced` is set, edits to the same path in the same
     session pass through GateGuard.

In short: read → cover → investigate-once → mutate. The model cannot
opt out of the read-and-cover step, and it cannot skip the
investigation FORCE prompt on its first edit to a given file.

### Shell: hard-deny vs. one-time soft-gate

Worker shell commands (`call_shell`) are classified for destructive
behavior **after** the existing `cargo_check_denied` cargo-steering step
and **before** any subprocess is constructed. The classifier returns
one of three outcomes (`ShellDestructiveDecision`):

- **Allow** — the command does not match any destructive pattern and
  executes normally.
- **HardDeny** — the command is unconditionally forbidden for workers.
  `bash_soft_forced` is **never** recorded for a hard-denied command,
  and FORCE / retry / a re-prompt / a second invocation in the same
  session cannot unlock it. The error explicitly states the command is
  "forbidden and cannot be unlocked by FORCE or retry."
- **SoftGate(DestructiveClass)** — the command is a lower-risk
  worktree-local mutation (e.g. `rm`, `mv`, `mkdir`, `touch`,
  `truncate`, `sed -i`, output redirection to a relative path). On the
  **first** invocation per `(live-session, DestructiveClass)` the worker
  is shown a FORCE prompt demanding **what files or data will be deleted
  or mutated** and a **one-line rollback or recreation plan**. The
  class is then recorded in `bash_soft_forced`. The **second**
  invocation of any command in the same class within the same live
  session proceeds without re-prompting.

**Hard-deny categories** (always forbidden, no FORCE override):

- VCS history/state destruction: `git reset --hard`, `git clean`,
  `git stash`, force-push (`git push --force` / `-f`), and other remote
  config mutation.
- DB DDL/DML through DB CLIs: `DROP TABLE`, `DELETE FROM`, `TRUNCATE`,
  `ALTER`, `INSERT`, `UPDATE` (and equivalents like `DROP DATABASE`,
  `DROP SCHEMA`).
- Package installs and publishes: `cargo install` / `cargo publish`,
  `pip install`, `npm install`, `apt install`, etc.
- Network mutation forms: `curl -X POST`, `curl -d` / `--data`,
  `wget --post-data`.
- Raw disk writes: `dd`, `install`.
- Path-scope exclusions: commands targeting `.git/`, parent directories
  (`..`), absolute paths, `.djinn/read-sources/`, or durable project
  data (`Cargo.toml`, `package.json`, `.gitignore`, …) are hard-denied
  even if the verb itself is in a normally soft-gated class.

**Soft-gate categories** (one-time FORCE plan, then allowed for the
class in the live session):

- `WorktreeLocalFileMutation` — the only soft-gate class the worker
  classifier currently emits; covers `rm`, `mv`, `mkdir`, `touch`,
  `truncate`, `sed -i`, `chmod`, `ln`, and output redirection to
  relative, non-protected paths.
- `VcsSoftGate` and `DbSoftGate` — reserved identifiers for future
  carve-outs. The FORCE-plan pattern (one-time per session, retry
  within the live session is allowed) is shared.

The FORCE prompt for a soft-gated command is the worker's only
opportunity to "unlock" the class. After the worker retries the command
in the live session with `bash_soft_forced` already set for the class,
the command executes without another FORCE prompt. A **hard-deny never
converts to a soft-gate on retry** — the two outcomes are distinct for
the entire live session.

### Role scope

GateGuard shell enforcement is **worker-only**. Reviewer, planner,
architect, and missing-role sessions bypass the shell classification
entirely; a reviewer's `rm` of a scratch file runs unchanged, and a
planner's destructive-class shell invocation never records
`bash_soft_forced`. Any role-independent hard-deny that already existed
(e.g. cargo steering for reviewer/worker) continues to apply —
GateGuard does not relax it.

This prose reflects the post-`46dt` / `0k63` / `0b46` behavior: the
classifier and dispatch wiring landed first, the dispatch and
path-scope tests pinned the behavior, and this document mirrors what the
tests guarantee.

## Tests

Tests run against a dedicated throwaway Postgres (not the dev cluster),
started via Docker Compose on `:5433`:

```bash
docker compose up -d postgres-test   # test-only Postgres
make test                            # djinn-db tests
make test-all                        # whole workspace (cargo nextest)
make sqlx-check                      # fail if the offline sqlx cache is stale
```

The workspace `.cargo/config.toml` defaults `DATABASE_URL` and
`DJINN_TEST_DATABASE_URL` to the `:5433` instance. `make test-all` rebuilds the
`djinn_test_template` database and then mirrors the merge-queue/full-suite
nextest command: `cd server && cargo nextest run --workspace --all-targets
--all-features --profile ci`. The PR-time fast gate is intentionally cheaper
(`cargo nextest ... --no-run` plus clippy); DB-backed test execution runs in
the merge queue/manual workflow.

The lifecycle/concurrency regressions for the vjs6 incidents (dispatch-cap
races, slot-pool lifecycle event races, `execution_kill_task` kill/cancel
settlement, and reopen/intervention chaos) are part of that unfiltered
`profile ci` full-suite path and must remain enabled. They use in-process
TestRuntime/template-Postgres or in-memory helpers, not kind/k8s;
stripped-down worker pods without Postgres/template setup may be unable to run
them locally. See
[`LIFECYCLE_CONCURRENCY_TEST_INVENTORY.md`](LIFECYCLE_CONCURRENCY_TEST_INVENTORY.md)
for the focused filters and rationale.
