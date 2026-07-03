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
(Node), and `openssl`.

The image pipeline runs BuildKit rootless via user namespaces. On the host
(kind inherits host sysctls):

```sh
sudo sysctl -w kernel.unprivileged_userns_clone=1
sudo sysctl -w user.max_user_namespaces=28633   # or higher
```

Then, from the repo root:

```bash
tilt up
```

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
