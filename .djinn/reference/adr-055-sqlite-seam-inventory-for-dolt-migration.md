---
title: ADR-055 SQLite seam inventory for Dolt migration
type: reference
tags: ["adr-055","sqlite","dolt","mysql","qdrant","migration","seams"]
---

# ADR-055 SQLite seam inventory for Dolt/MySQL + Qdrant

Originated from task `019d8915-2695-7593-83a9-4226231a6675` (`mved`).

## Purpose

This note inventories the concrete SQLite-coupled seams in the current `djinn-db` stack so follow-on ADR-055 work can replace them with explicit backend boundaries instead of broad ad hoc edits.

Primary evidence reviewed:

- `server/crates/djinn-db/src/database.rs`
- `server/crates/djinn-db/src/migrations.rs`
- `server/crates/djinn-db/schema.sql`
- `server/crates/djinn-db/src/repositories/note/search.rs`
- `server/crates/djinn-db/src/repositories/note/embeddings.rs`
- targeted grep across `server/crates/djinn-db/src/**` and `server/crates/djinn-db/migrations/**`
- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]

## Executive summary

SQLite is not isolated to one bootstrap module. The current coupling spans five layers:

1. **Database runtime/bootstrap**: `Database` is hard-coded around `sqlx::SqlitePool`, `SqliteConnectOptions`, SQLite URIs, pragmas, and `rusqlite` migration execution.
2. **DDL and migration strategy**: canonical schema and many migrations depend on SQLite-only DDL, timestamp functions, virtual tables, triggers, and table-rebuild patterns guarded by `PRAGMA foreign_keys` toggles.
3. **Lexical search**: note search is directly built around FTS5 syntax, `MATCH`, `bm25(...)`, and `rowid` joins to the `notes_fts` virtual table.
4. **Semantic vector storage**: embedding persistence assumes local blob storage plus `sqlite-vec` `vec0` virtual table initialization, availability checks, and nearest-neighbor query syntax.
5. **Repository query semantics**: many repositories assume SQLite date/time functions, JSON table functions, and SQL dialect details that will need a SQL-dialect seam even when the high-level repository API stays the same.

The main migration risk is not just changing connection strings. It is that the current repositories expose behavior that already bakes in SQLite-specific ranking, vector availability, temporal math, and JSON filtering.

## Seam inventory

### 1. Bootstrap and connection-management seam

#### Concrete SQLite couplings

**`server/crates/djinn-db/src/database.rs`**

- `Database` owns a `pool: SqlitePool` and exposes `pub fn pool(&self) -> &SqlitePool`.
- Constructors use `SqliteConnectOptions::from_str("sqlite://...")` in:
  - `Database::open`
  - `Database::open_readonly`
  - `Database::open_in_memory`
- `.foreign_keys(true)` is set directly on SQLite connect options.
- connection initialization uses SQLite-only hooks:
  - `apply_pragmas`
  - `apply_pragmas_readonly`
- `apply_pragmas` issues:
  - `PRAGMA journal_mode = WAL`
  - `PRAGMA busy_timeout = 30000`
  - `PRAGMA synchronous = NORMAL`
  - `PRAGMA foreign_keys = ON`
  - `PRAGMA cache_size = -64000`
- tests assert SQLite runtime state via `PRAGMA journal_mode`, `PRAGMA busy_timeout`, `PRAGMA synchronous`, and `PRAGMA foreign_keys`.
- `default_db_path()` assumes a single local file database at `~/.djinn/djinn.db`.

#### Why this is a migration seam

Dolt/MySQL will need networked or daemon-backed connection setup, different pool/session initialization, no pragma concept, and likely per-session branch checkout. Qdrant should not be represented as a SQLite extension health bit attached to the relational pool.

#### Initial abstraction boundary

Create a **database backend/runtime seam** with responsibilities split into:

- `RelationalBackend` / `DatabaseBackend` for pool creation and initialization
- `SessionConfigurator` for backend-specific session defaults (`PRAGMA ...` vs `SET ...` / Dolt checkout)
- `DatabaseLocator` for local SQLite path vs Dolt DSN configuration

`Database` should stop exposing `&SqlitePool`; callers should depend on backend-neutral repository capabilities or an internal executor wrapper.

---

### 2. Migration-runner seam

#### Concrete SQLite couplings

**`server/crates/djinn-db/src/migrations.rs`**

- `run(path: &Path)` opens the DB with `rusqlite::Connection::open(path)`.
- refinery migrations are executed through the **rusqlite runner**.
- `run_until(...)` test helper also depends on `rusqlite` and refinery version targeting.

**`server/crates/djinn-db/src/database.rs`**

- `ensure_initialized()` calls `tokio::task::spawn_blocking(move || migrations::run(&db_path))`.
- initialization sequencing assumes a file path that a blocking `rusqlite` migration runner can open separately from `sqlx`.
- test fixture helpers (`create_legacy_note_fixture_db`) create partially migrated SQLite files through `rusqlite`.

#### Why this is a migration seam

Dolt/MySQL migrations cannot reuse a local-file `rusqlite` runner. The current API also assumes initialization = “run migrations against a file path before async use.” Dolt likely needs DSN-based migrations, possibly different migration tooling, and branch-aware bootstrap rules.

#### Initial abstraction boundary

Create a **schema migrator seam**:

- `SchemaMigrator::migrate(target)`
- `SchemaBootstrapTarget` should be DSN/backend oriented, not `Path`
- test helpers should move behind a backend fixture trait

This seam should absorb:

- migration transport (`rusqlite` vs MySQL driver)
- migration source format
- backend bootstrap ordering

---

### 3. Canonical schema / DDL seam

#### Concrete SQLite couplings

**`server/crates/djinn-db/schema.sql`**

SQLite-specific DDL appears in several categories:

1. **timestamp defaults and time functions**
   - widespread `DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))`
   - examples across `settings`, `projects`, `tasks`, `notes`, `note_embeddings`, `note_links`, `sessions`, `verification_cache`, `note_associations`, `consolidated_note_provenance`, etc.
2. **FTS5 virtual table**
   - `CREATE VIRTUAL TABLE notes_fts USING fts5(...)`
3. **trigger-based FTS synchronization**
   - `notes_ai`, `notes_ad`, `notes_au`
   - all rely on `rowid`
4. **SQLite row identity assumptions**
   - FTS table sync stores `rowid` and joins through `notes.rowid`
5. **SQLite type/storage conventions**
   - JSON-like columns stored as `TEXT` (`labels`, `tags`, `scope_paths`, payloads)
   - booleans represented as `INTEGER NOT NULL DEFAULT 0/1`
   - `BLOB` embedding storage in `note_embeddings.embedding`
6. **CHECK / rebuild compatibility expectations**
   - current schema and migrations often tolerate SQLite’s table rebuild style more than MySQL-style `ALTER TABLE`

#### Why this is a migration seam

ADR-055 replaces:

- `notes_fts` with MySQL/Dolt `FULLTEXT`
- `note_embeddings_vec` with Qdrant
- SQLite timestamps/functions with MySQL-compatible defaults or app-side timestamps
- trigger-maintained denormalized FTS state with either generated/indexed columns or app-driven indexing

#### Initial abstraction boundary

Split schema concerns into three explicit seams:

- **relational canonical schema** (portable tables only)
- **lexical index schema adapter** (`FTS5` vs `FULLTEXT`)
- **vector index schema adapter** (SQLite local table vs external Qdrant collection metadata)

Do not keep search/vector DDL embedded in the same “canonical schema snapshot” abstraction.

---

### 4. Lexical search seam (FTS5)

#### Concrete SQLite couplings

**`server/crates/djinn-db/src/repositories/note/search.rs`**

- `sanitize_fts5_query(raw: &str)` explicitly targets **FTS5 syntax**.
- Query building assumes `MATCH` against the `notes_fts` virtual table.
- Ranking assumes SQLite `bm25(notes_fts, 3.0, 1.0, 2.0)` weights.
- `dedup_candidates(...)`:
  - selects from `notes_fts`
  - joins `notes n ON notes_fts.rowid = n.rowid`
  - filters with `WHERE notes_fts MATCH ?1`
  - thresholds on negated BM25 score
- `detect_contradiction_candidates(...)` repeats the same coupling.
- `search(...)` uses FTS5 as the lexical candidate generator before RRF fusion.
- `server/crates/djinn-db/src/repositories/note/consolidation.rs` also uses `sanitize_fts5_query`, `notes_fts`, and `bm25(...)`.
- tests in `server/crates/djinn-db/src/repositories/note/tests/search_ranking.rs` are named for FTS5 behavior and encode title/content/tag weighting assumptions.

#### Why this is a migration seam

MySQL `FULLTEXT ... AGAINST` is not syntax-compatible with FTS5. There is no direct `bm25(notes_fts, title_weight, content_weight, tag_weight)` equivalent and there is no `rowid` join through an auxiliary virtual table.

#### Initial abstraction boundary

Introduce a **lexical search backend seam** at the repository layer:

- `LexicalNoteSearchBackend`
  - `candidate_scores(project_id, query, filters, limit)`
  - `dedup_candidates(...)`
  - `contradiction_candidates(...)`
- `QuerySanitizer` should become backend-specific (`sanitize_fts5_query` cannot be reused for boolean-mode MySQL FULLTEXT).
- Keep **RRF fusion** in shared repository code, but treat lexical retrieval as one pluggable signal source.

This boundary allows the sibling task for MySQL FULLTEXT to replace only candidate generation while preserving downstream ranking fusion.

---

### 5. Semantic vector seam (`sqlite-vec` + local metadata)

#### Concrete SQLite couplings

**`server/crates/djinn-db/src/database.rs`**

- `SqliteVecStatus` is a database-level concept.
- auto-extension registration depends on:
  - `rusqlite::ffi::sqlite3_auto_extension`
  - `sqlite_vec::sqlite3_vec_init`
- `initialize_sqlite_vec(...)` executes `SELECT vec_version()`.
- it creates `note_embeddings_vec` with:
  - `CREATE VIRTUAL TABLE IF NOT EXISTS note_embeddings_vec USING vec0(...)`
- extension availability is cached on the `Database` instance.

**`server/crates/djinn-db/src/repositories/note/embeddings.rs`**

- `sync_note_embedding(...)` skips embedding generation entirely unless `sqlite_vec_status().available` is true.
- `upsert_embedding(...)` writes to both:
  - `note_embeddings` BLOB storage
  - `note_embedding_meta`
  - `note_embeddings_vec` virtual table when vec0 is available
- `delete_embedding(...)` conditionally deletes from `note_embeddings_vec`.
- `query_similar_embeddings(...)` issues sqlite-vec syntax:
  - `SELECT note_id, distance FROM note_embeddings_vec WHERE embedding MATCH ?1 AND k = ?2`
- `semantic_candidate_scores(...)` assumes the raw vector backend returns note IDs that must then be filtered in SQL.
- tests in `server/crates/djinn-db/src/repositories/note/tests/embeddings.rs` explicitly toggle `set_sqlite_vec_disabled_for_tests` and assert `sqlite_vec_status` behavior.

#### Why this is a migration seam

ADR-055 moves nearest-neighbor search out of SQLite entirely and into Qdrant. Availability, collection setup, filtering, and delete/upsert behavior all need to move out of the relational DB runtime.

There is also a hidden product seam here: current code conflates three responsibilities:

1. embedding generation
2. relational metadata persistence
3. vector index persistence/querying

#### Initial abstraction boundary

Introduce a **vector store seam** with a narrow API:

- `VectorStore`
  - `upsert_note_embedding(note_id, vector, payload)`
  - `delete_note_embedding(note_id)`
  - `query_similar(vector, filters, limit)`
  - `health()`

Keep relational metadata in a separate `NoteEmbeddingMetadataRepository` (or repository submodule) so Qdrant migration does not have to preserve local `BLOB` storage semantics.

Also split `SqliteVecStatus` into a backend-neutral **semantic index health** concept.

---

### 6. Repository SQL-dialect seam beyond search/vector features

The task asked for places where repository APIs currently assume SQLite semantics. The following are the highest-leverage non-search seams.

#### 6a. Time arithmetic and timestamp formatting

Concrete references from grep across `server/crates/djinn-db/src/**`:

- `verification_result.rs`, `verification_cache.rs`, `settings.rs`, `git_settings.rs`, `session.rs`, `task/status.rs`, `task/writes.rs`, `note/crud.rs`, `note/graph.rs`, `note/housekeeping.rs`, `agent.rs`, and others use:
  - `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')`
  - `datetime('now', '-N days')`
  - `datetime('now', '-N hours')`
- `search.rs` uses `updated_at >= datetime('now', '-{hours} hours')`.
- tests in `note/tests/graph_scoring.rs` and `repositories/test_support.rs` mutate timestamps with `datetime('now', ...)`.

Why it matters:

- MySQL/Dolt date arithmetic syntax differs.
- app-visible ISO timestamp formatting is currently delegated to SQLite.

Recommended boundary:

- move toward an **app-side clock/timestamp formatter seam** for writes
- centralize date-window predicates in a **dialect helper** or query builder rather than embedding raw `strftime/datetime` strings in repositories

#### 6b. JSON table functions over TEXT columns

Concrete references:

- `search.rs` scope overlap uses `json_each(n.scope_paths)`.
- `task/queries.rs` label filtering uses `EXISTS (SELECT 1 FROM json_each(...))`.
- comments already note virtual table scan behavior in `task/reads.rs`.

Why it matters:

- current APIs assume JSON arrays are stored as SQLite `TEXT` blobs and queried with SQLite’s JSON1 table-valued functions.
- MySQL JSON filtering syntax and indexing strategy differ materially.

Recommended boundary:

- introduce repository-local helpers for **array membership / overlap predicates**
- consider whether some current JSON-text columns should become join tables or native JSON columns during Dolt migration

#### 6c. SQLite-specific table rebuild migrations

Concrete references in migrations:

- many migrations toggle `PRAGMA foreign_keys = OFF/ON` while rebuilding tables, including:
  - `V20260322000001__task_status_pr_states.sql`
  - `V20260320000005__rename_pm_to_lead_statuses.sql`
  - `V20260320000002__add_pr_ready_status.sql`
  - `V20260320000001__remove_issue_type_check_constraint.sql`
  - `V20260319000010__task_issue_type_drop_check.sql`
  - `V20260319000002__remove_backlog_status.sql`
  - `V20260313000001__epic_remove_in_review_status.sql`
  - `V20260312000001__rebuild_tasks_add_backlog_status.sql`
  - `V20260408000001__epic_proposed_and_breakdown_fields.sql`
- these are tailored to SQLite’s limited `ALTER TABLE` capabilities.

Why it matters:

- Dolt/MySQL migration work should not attempt one-for-one translation of every historical SQLite rebuild migration.
- there needs to be a decision whether Dolt bootstrap is from a fresh canonical schema, a new migration chain, or a one-time import path.

Recommended boundary:

- separate **historical SQLite migration preservation** from **new backend bootstrap**
- treat Dolt bootstrap as a new migration lineage rather than forcing refinery/rusqlite history to remain authoritative

---

## Implementation work buckets

### Bucket A — backend bootstrap and migrator extraction

Scope:

- `src/database.rs`
- `src/migrations.rs`
- tests and fixture helpers that assume local SQLite files

Deliverables:

- backend-neutral database bootstrap API
- backend-specific session initialization hooks
- migrator abstraction that no longer takes only `Path`

Feeds sibling task:

- `y70d` — refactor database bootstrap for selectable SQLite vs Dolt/MySQL backends

### Bucket B — lexical search backend split

Scope:

- `src/repositories/note/search.rs`
- `src/repositories/note/consolidation.rs`
- FTS-oriented tests in `src/repositories/note/tests/search_ranking.rs`
- `schema.sql` FTS table and triggers

Deliverables:

- lexical candidate provider seam
- backend-specific query sanitizer
- preserved shared RRF fusion and note hydration path

Feeds sibling task:

- `keit` — prototype Dolt/MySQL FULLTEXT notes search to replace FTS5

### Bucket C — vector store extraction

Scope:

- `src/database.rs` sqlite-vec registration/status
- `src/repositories/note/embeddings.rs`
- `migrations/V20260413000001__add_note_embeddings.sql`
- embedding tests built around sqlite-vec enable/disable state

Deliverables:

- vector-store trait and health model
- separation of embedding metadata from vector index storage
- Qdrant scaffold path

Feeds sibling task:

- `6iiz` — introduce vector-store abstraction and Qdrant scaffold for note embeddings

### Bucket D — SQL dialect and portability helpers

Scope:

- repository SQL using `strftime`, `datetime`, `json_each`, and SQLite-specific boolean/timestamp conventions
- representative files include `task/queries.rs`, `task/status.rs`, `note/search.rs`, `note/crud.rs`, `note/graph.rs`, `note/housekeeping.rs`, `agent.rs`, `verification_cache.rs`, `verification_result.rs`

Deliverables:

- dialect helper layer or query helpers for:
  - current timestamp
  - time-window filters
  - JSON array membership / overlap predicates
- decision on which current TEXT/JSON columns should remain JSON vs become normalized tables in Dolt

This is broader than the first migration wave, but identifying it now prevents later surprises once search/vector work lands.

### Bucket E — schema/bootstrap strategy for Dolt branch-aware runtime

Scope:

- canonical schema design for Dolt/MySQL
- replacing trigger-maintained FTS and local vector tables
- planning for per-task branch checkout during connection/session setup

Deliverables:

- split relational schema from search/vector indexing schema
- choose app-side vs DB-side timestamp ownership
- ensure branch checkout is owned by backend/session layer, not individual repositories

Feeds sibling task:

- `4hkv` indirectly, because branch-aware reads/writes must sit below repository call sites

## Recommended first abstraction cuts

### 1. `DatabaseBackend`

A backend object should own:

- pool creation
- session initialization
- health reporting
- branch/session checkout behavior
- backend-specific initialization side effects

This is the first seam because current `Database` shape leaks SQLite throughout the repository layer.

### 2. `SchemaMigrator`

Do not keep migrations as a `rusqlite` implementation detail hidden behind `Database::ensure_initialized()`. Make migration/bootstrap an explicit backend concern.

### 3. `LexicalNoteSearchBackend`

This isolates FTS5/MySQL FULLTEXT differences while preserving repository-visible search behavior and RRF fusion.

### 4. `VectorStore`

This should become the only owner of nearest-neighbor persistence and query semantics. Repositories should not know about `vec0`, `MATCH ? AND k = ?`, or Qdrant request shapes.

### 5. `SqlDialect` / query helper module

A light abstraction for time and JSON predicates will pay off quickly because SQLite date and JSON functions are scattered across many repositories.

## Suggested migration order

1. **Extract bootstrap/migrator seam first** so follow-on tasks do not deepen `SqlitePool` exposure.
2. **Extract vector store next** because sqlite-vec is the cleanest service boundary and already has a clear ADR-055 replacement (Qdrant).
3. **Split lexical search backend** while keeping RRF unchanged.
4. **Centralize dialect helpers** for timestamps/JSON predicates before broad Dolt query conversion.
5. **Port canonical schema** after search/vector seams are no longer embedded in a single SQLite-first schema snapshot.

## Non-goals for the first follow-on tasks

- Do not generalize every repository to a fully generic SQL engine immediately.
- Do not preserve one-for-one compatibility with every historical SQLite migration.
- Do not let Qdrant concerns leak into note CRUD APIs; keep them behind `VectorStore`.
- Do not tie lexical ranking fusion logic to a specific backend’s raw scoring semantics.

## Relations

- [[decisions/adr-055-proposal-dolt-migration-and-per-task-knowledge-branching]]
- [[roadmap]]
