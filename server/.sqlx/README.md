# sqlx offline mode (compile-time-checked queries)

Some hot-path queries in this workspace use `sqlx::query!` / `sqlx::query_as!`,
which verify SQL against the live database schema at **compile time**. To keep
`cargo check`/`cargo build` working without a running Dolt instance (e.g. on CI
or in clean clones), we use sqlx's **offline mode** backed by the `.sqlx/`
directory of prepared query metadata.

## Install sqlx-cli (once)

```
cargo install sqlx-cli --no-default-features --features mysql,rustls
```

## Regenerate `.sqlx/` metadata

With the test Dolt running on :3307 (started by `docker compose up -d dolt-test`),
run from the repo root:

```
make sqlx-prepare
```

This invokes `cargo check --workspace --all-targets --all-features` with
`SQLX_OFFLINE_DIR` pointing at a tmpdir, so every `query!`/`query_as!` call —
including those inside `#[cfg(test)]` blocks — writes a JSON into the cache.
The tmpdir then replaces `.sqlx/` atomically.

Do **not** use `cargo sqlx prepare --workspace` directly: as of sqlx-cli 0.8.6
it doesn't compile test targets, so queries inside `#[cfg(test)]` modules silently
miss the cache and break CI's offline build.

Regenerate `.sqlx/` whenever you:

- Add or change a `query_as!` / `query!` call.
- Alter schema (new migrations, column renames, type changes).

## Offline mode at build time

With a populated `.sqlx/` directory committed to git, `cargo check` /
`cargo build` work without a database. To force offline mode even when a DB is
reachable:

```
SQLX_OFFLINE=true cargo check
```

## `.sqlx/` is committed to git

Unlike a typical build artifact directory, `.sqlx/` **should be checked in** —
it is the offline schema cache that lets clean clones build without Dolt.
Do **not** add `.sqlx/` to `.gitignore`.
