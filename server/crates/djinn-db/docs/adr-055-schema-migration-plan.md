# ADR-055 schema migration plan for Dolt/MySQL

Originated from task `019d891d-dc2c-7421-8763-395380c02029`.

## Goal

Provide a concrete, inspectable migration target for the note/task/session relational state needed
by ADR-055 without breaking the current SQLite runtime.

## Current selectable backend

- **SQLite remains the active runtime**.
- `server/src/db/runtime.rs` still defaults to `sqlite` and preserves SQLite-specific bootstrap
  behavior such as local pragmas and embedded refinery migrations.
- Selecting `mysql` or `dolt` is already explicit through `DJINN_DB_BACKEND`, but repository
  execution remains staged until the cutover lands.

## Staged MySQL/Dolt artifacts

Two concrete artifacts now define the migration target:

1. `server/crates/djinn-db/sql/mysql_schema.sql`
   - full relational schema snapshot for the ADR-055 note/task/session state
   - includes `projects`, `tasks`, `notes`, `sessions`, `session_messages`,
     `task_memory_refs`, and related note-link / provenance tables needed by current workflows
2. `server/crates/djinn-db/sql/mysql_notes_fulltext_prototype.sql`
   - reference search SQL using `MATCH ... AGAINST` over `notes`

## SQLite-only elements now isolated

The SQLite path continues to own these features in `schema.sql` and refinery migrations:

- `notes_fts` FTS5 virtual table
- trigger-based synchronization into `notes_fts`
- pragma-driven connection setup
- optional `sqlite-vec` runtime initialization

The MySQL/Dolt schema snapshot intentionally omits those features and replaces them with:

- a native `FULLTEXT` index on `notes(title, content, tags)`
- ordinary relational embedding tables (`note_embeddings`, `note_embedding_meta`)
- branch metadata on `note_embedding_meta.branch` so canonical (`main`) and task-local
  (`task/<short_id>`) vectors can be filtered together during retrieval and promoted or deleted
  during task completion / discard flows without depending on `sqlite-vec` tables
- no trigger-maintained shadow tables

## Branch-aware semantic retrieval contract

- Task-session note writes infer an embedding branch from the worktree root (`.djinn/worktrees/<short_id>`)
  and persist vectors/metadata under `task/<short_id>`.
- Semantic retrieval composes `task/<short_id>` plus `main` by filtering embedding metadata on the
  allowed branch set before RRF fusion with lexical, temporal, graph, and task-affinity signals.
- Knowledge promotion updates embedding metadata from `task/<short_id>` to `main`; discard / abandon
  deletes embeddings for the task branch entirely.
- Qdrant collections should mirror this contract via payload fields keyed by `note_id`,
  `content_hash`, `model_version`, and `branch`, with a payload index on `branch` for fast
  `task/<short_id>` + `main` filtered queries.

## Verification hooks

`server/crates/djinn-db/src/migrations.rs` now exposes both schema snapshots and contains tests
asserting that:

- the SQLite snapshot still includes FTS5 + trigger sync
- the MySQL snapshot uses `FULLTEXT` and excludes FTS5 / trigger / `vec0` structures
- the MySQL schema and prototype together cover `tasks`, `notes`, and `sessions`

## Intended cutover sequence

1. Keep SQLite refinery migrations as-is for the current runtime.
2. Use `sql/mysql_schema.sql` as the authoritative relational target while MySQL repository support
   is implemented.
3. Switch lexical note search to the existing backend-aware planning seam plus the
   `mysql_notes_fulltext_prototype.sql` query shape.
4. Introduce backend-specific migrators once repository execution can run on MySQL/Dolt.

This keeps the SQLite path selectable today while making the Dolt/MySQL schema path concrete and
unambiguous.
