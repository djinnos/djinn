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

Run from the workspace root with Dolt running locally:

```
DATABASE_URL=mysql://root@127.0.0.1:3306/djinn cargo sqlx prepare --workspace
```

This scans every crate's `query!`/`query_as!` call, asks the DB for schema info,
and writes one JSON file per query into `.sqlx/`. Commit the results.

Regenerate `.sqlx/` whenever you:

- Add or change a `query_as!` / `query!` call.
- Alter schema (new migrations, column renames, type changes).

CI should run `cargo sqlx prepare --workspace --check` to fail if metadata is
stale.

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
