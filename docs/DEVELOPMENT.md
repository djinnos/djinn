# Development

The Rust workspace lives in `server/` (binary `djinn-server` plus ~16 crates
under `server/crates/`); the web client is in `ui/` (React + Vite +
TypeScript, pnpm). The UI is compiled into the server binary via `rust-embed`,
and the TypeScript MCP types are generated from the server's live tool schemas
(`pnpm --dir ui mcp:types`).

`tilt up` is the dev loop — it brings up the full stack in a local kind
cluster and hot-reloads binaries and UI from your working tree. See
[Quick start (local)](../README.md#quick-start-local).

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
