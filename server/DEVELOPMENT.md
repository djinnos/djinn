# Djinn server development

## Database

Runtime: **Dolt** (MySQL wire protocol), managed by `docker compose`.
Schema: `server/crates/djinn-db/migrations_mysql/*.sql`, applied by
`sqlx::migrate!()` on server boot.
History lives in `_sqlx_migrations`; checksums are enforced — mutating
an applied migration makes the server refuse to start.

## Queries are compile-time checked

Queries in `djinn-db/src/repositories/` use `sqlx::query!` /
`sqlx::query_as!` / `sqlx::query_scalar!` macros. The macros parse your
SQL against the **live database schema at `cargo build` time** and fail
the build if column names or types don't match the Rust struct.

Two paths exist:

| Path | When | Needs live DB |
|------|------|--------|
| Online | Local dev during active query changes | Yes — `DATABASE_URL` points at Dolt |
| Offline | CI, Docker builds, contributors without Dolt | No — reads `server/.sqlx/` |

### Local workflow

1. Boot the stack: `make up` (starts Dolt + qdrant + djinn-server with latest code).
2. Write or modify a query in a repository file.
3. Regenerate the offline cache: `make sqlx-prepare`.
4. Commit both the code change *and* the `server/.sqlx/` delta in the same commit.

Tools needed once: `cargo install sqlx-cli --no-default-features --features mysql,rustls`.

### Troubleshooting

**`error: error returned from database: no such table: …`** during `cargo
build` — the offline cache was generated against an older schema. Run
`make sqlx-prepare` then retry.

**`error: sqlx offline mode requires the SQLX_OFFLINE env var …`** — you
built with `SQLX_OFFLINE=true` but have no `server/.sqlx/` committed.
Someone else forgot to regen. Run `make sqlx-prepare`.

**`error: mismatched types: Rust type `…` is not compatible with SQL type
`…``** — exactly the bug this system prevents. Read the macro's
diagnostic — it points at the file, line, and column. Fix the struct or
the SQL.

## Migrations

See `migrations_mysql/1_initial_schema.sql` for the current baseline.
Add new migrations as `<N>_<slug>.sql` with strictly increasing `N`.
**Never modify an applied migration** — checksum enforcement will
refuse startup and CI (via `sqlx-check`) will fail.

Example, adding a column:

```
# 1. edit schema
$EDITOR server/crates/djinn-db/migrations_mysql/3_add_my_column.sql
# 2. apply + regen macros
make up              # server auto-applies on boot
make sqlx-prepare    # rebuild offline cache to match
# 3. commit both files in one commit
git add server/crates/djinn-db/migrations_mysql/3_add_my_column.sql server/.sqlx
git commit -m "feat(db): add my_column to foo"
```

## CI

CI runs `make sqlx-check` against a booted Dolt. Any drift between the
`.sqlx/` cache and the current queries fails the job.

## Running tests

Single crate: `cargo test -p <crate>` (or `make test` for just djinn-db).
**Do not run `cargo test --workspace`** — each crate spins up 100–250 fresh
`djinn_test_*` databases and Dolt caches them all, saturating the 8 GiB
test-Dolt and cascading into `UnexpectedEof` failures. Run the full suite
with `make test-all`, which runs each crate sequentially with a
`test-db-reset` between them to drain the cache.
