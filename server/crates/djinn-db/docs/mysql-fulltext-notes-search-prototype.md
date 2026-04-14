# MySQL FULLTEXT notes search prototype

Originated from task `019d8915-26c8-79c0-a391-5adf2d4eca85`.

This prototype captures the repository and schema changes needed to replace
SQLite FTS5/BM25 note search with Dolt/MySQL `FULLTEXT` per ADR-055.

## Current SQLite-specific assumptions

The current note lexical pipeline depends on SQLite-only features in four places:

1. `migrations/V20260303000002__notes.sql`
   - creates `notes_fts` as an FTS5 virtual table
   - keeps it in sync with triggers
2. `src/repositories/note/search.rs`
   - sanitizes user text into FTS5 `MATCH` syntax
   - ranks via `bm25(notes_fts, 3.0, 1.0, 2.0)`
   - treats better matches as **lower** BM25 values, then negates scores in some callers
3. `src/repositories/note/context.rs`
   - uses the same FTS5+BM25 candidate generation for build-context discovery
4. `src/repositories/note/consolidation.rs`
   - dedup clustering reuses FTS5 with a threshold tuned to negated BM25 scores

## Prototype seam added in Rust

`src/repositories/note/lexical_search.rs` now defines a backend-neutral planning seam:

- `LexicalSearchBackend::{SqliteFts5, MysqlFulltext}`
- `LexicalSearchMode::{Ranked, Dedup, Contradiction, Discovery}`
- `build_lexical_search_plan(...)`

This seam does not switch runtime execution yet. Instead, it makes the cutover concrete by
encoding the SQL shape, sanitization behavior, ordering direction, and threshold caveats for
both backends.

## MySQL/Dolt replacement shape

### Schema

Instead of the `notes_fts` shadow table and triggers, add:

```sql
ALTER TABLE notes ADD FULLTEXT INDEX notes_ft (title, content, tags);
```

### Query model

Use `MATCH(title, content, tags) AGAINST (? IN BOOLEAN MODE)` directly on `notes`.

Important differences from SQLite:

- no shadow table join
- no trigger maintenance
- relevance is **higher score is better**
- thresholds must be recalibrated because MATCH() scores are not BM25-compatible

## Follow-on implementation guidance

When the MySQL backend lands, the repository cutover should:

1. replace `sanitize_fts5_query` usages with backend-aware sanitization
2. route lexical queries through the backend planner seam
3. retune these constants empirically for MySQL:
   - dedup threshold (`-3.0` today in BM25-space)
   - contradiction threshold (`5.0` today in negated BM25-space)
4. keep the downstream contracts unchanged:
   - lexical candidate generation feeds RRF
   - contradiction still returns top 3 before `TypeRisk` filtering
   - context discovery still returns lexical candidates only

## Included reference artifacts

`sql/mysql_notes_fulltext_prototype.sql` contains executable reference SQL for:

- ranked search
- dedup candidates
- contradiction candidates
- build-context lexical discovery

`sql/mysql_schema.sql` now accompanies that prototype with a concrete MySQL/Dolt schema snapshot for
the ADR-055 note/task/session tables. The schema explicitly:

- keeps `tasks`, `notes`, `sessions`, `task_memory_refs`, and adjacent relational tables in a
  MySQL-compatible layout
- replaces SQLite FTS5 shadow tables and triggers with `ALTER TABLE notes ADD FULLTEXT KEY`
- keeps embedding bytes in ordinary relational tables instead of depending on `sqlite-vec`

Both artifacts are intentionally stored outside refinery migrations so the current SQLite
test/runtime path remains unaffected while the Dolt/MySQL backend is still in progress.

## Backend selection and migration path

The repository now exposes both schema snapshots in Rust via `djinn_db::sqlite_schema_snapshot()`
and `djinn_db::mysql_schema_snapshot()`. Tests in `src/migrations.rs` verify the split:

- the SQLite snapshot still contains `notes_fts` and trigger-driven sync
- the MySQL snapshot contains `FULLTEXT` indexing and omits SQLite-only virtual tables/triggers

This matches the runtime seam introduced in `server/src/db/runtime.rs`: SQLite remains the default
selectable backend today, while `mysql`/`dolt` selection is explicit and can be paired with the
staged MySQL schema artifact without ambiguity.
