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
