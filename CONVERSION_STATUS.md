# SQLx compile-time conversion status

Branch: `feat/sqlx-compile-time-convert` (batch 1) + `feat/sqlx-compile-time-convert-batch2` (batch 2) + `feat/sqlx-compile-time-convert-batch3` (batch 3) + `feat/sqlx-compile-time-convert-batch4` (batch 4).

## Batch 4 summary

Phase A: Established the **inline-projection pattern** for shared SELECT
constants. `sqlx::query_as!` requires a string-literal SQL argument and
rejects `concat!()`-built literals (verified empirically — fails with
"expected string literal"). The `AGENT_COLUMNS`, `EPIC_COLS`, and
`SESSION_COLS` consts that fed `format!("SELECT {COLS} FROM …")` queries are
therefore expanded inline at every call site as raw string literals; each
copy carries the necessary `!: bool` / `!: T` / backtick-reserved-word
overrides for `is_default`, `auto_breakdown`, `status`, etc.

Phase B (files touched):

| File | Converted this batch | Annotated runtime | Notes |
|------|---------------------:|------------------:|-------|
| `repositories/note/graph.rs`              | 12 | 0 | Tuple `query_as` → named `query!()` rows for graph/orphan/broken-link/health helpers; SQLite-vec orphan check now uses `EXISTS … AS "exists!: i64"`. |
| `repositories/note/tests/graph_scoring.rs`| 0  | 16 | SQLite-only fixture (`datetime('now')`, `strftime`, positional `?N`); annotated at file head. |
| `repositories/note/consolidation.rs`      | 14 | 2  | DBNoteGroup/ConsolidationNote SELECTs use `\`abstract\` AS abstract_`; `INT` columns cast to `SIGNED` for `i64` binding; FTS dedup query gains explicit `f64` cast on MATCH() AGAINST and reorders ORDER BY by the expression. Two `format!()` IN-list helpers stay annotated. |
| `repositories/note/search.rs`             | 5  | 8  | Static helpers (`semantic_branch_for_task`, `catalog`, `task_refs`, two test INSERTs) macro-typed; lexical-search dispatch (SQLite/MySQL) and JSON_TABLE scope-overlap stay dynamic. |
| `repositories/note/association.rs`        | 22 | 0  | All static SELECT/INSERT/UPDATE/DELETE; `NoteAssociationEntry` CASE-WHEN columns get `!: String` overrides; tests + Project lookup use the standard `auto_merge!: bool` / `sync_enabled!: bool` overrides. |
| `repositories/agent.rs`                   | 24 | 2  | Inlined `AGENT_COLUMNS`; aggregate metrics queries (`get_metrics`, `count_closed_tasks_since`, `get_windowed_metrics`) replace tuple destructures with named `query!()` rows + explicit `!: f64` / `!: i64` overrides. `list_for_project`'s two `format!()` queries stay annotated. |
| `repositories/epic.rs`                    | 25 | 4  | Inlined `EPIC_COLS`; `task_counts` uses named `query!()` row + `COALESCE` so SUM(CASE WHEN …) bind without `sqlx::Row::get`; removed unused `use sqlx::Row;`. `list_filtered`/`count_grouped`/`short_id_exists` stay dynamic. |
| `repositories/session.rs`                 | 26 | 1  | Inlined `SESSION_COLS`; `event_taxonomy`/`worktree_path` scalar lookups now flatten `Option<Option<String>>`. `count_for_tasks` IN-list stays annotated. |
| `repositories/project.rs`                 | 24 | 0  | All UPDATE/INSERT/DELETE + `ProjectConfig` SELECT macro-typed; `list_sync_enabled` + the test `is_default` row reshape to use named `query!()` rows. |

Batch-4 new compile-time-checked call sites: **~152**.

**Converted total (cumulative) end of batch 4**: **~310** out of ~355 queries.
**Annotated-as-dynamic (cumulative)**: **~93** (new annotations: dynamic IN
lists in note/search, note/consolidation, session, plus the SQLite-only
`graph_scoring` fixture).
**`.sqlx/` cache**: 272 entries (from 144 at end of batch 3, +128 new).

### Documented constraints

- **`concat!()` does not satisfy `sqlx::query_as!` literal requirement.**
  Confirmed in batch 4 on `agent.rs` — fails with `expected string literal`.
  The shared-projection consts (`AGENT_COLUMNS`, `EPIC_COLS`, `SESSION_COLS`)
  were therefore inlined at every call site rather than wrapped in a
  `macro_rules!`. The `note_select_where_id!` and `task_select_where_id!`
  macros (batch 3) still work because they expand into the **complete**
  `query_as!(…, "literal", $id)` invocation in a single pass, with the SQL
  itself appearing as a single literal token inside the macro body.



## Batch 3 summary

Phase A: Converted `NOTE_SELECT_WHERE_ID` and `TASK_SELECT_WHERE_ID`
from `const &str` to full `macro_rules!` macros that expand directly
into `sqlx::query_as!(T, "...", id)` calls. Critical detail: sqlx's
`query_as!` demands a string-literal SQL argument at the token level
— neither `concat!` nor macro-produced literals satisfy it. The
macros take the id as an expression parameter and inline the query
themselves. All ~17 NOTE call sites (crud, indexing, consolidation,
tests/crud_storage) and ~20 TASK call sites (mod, reads, status,
writes, queries) now use the macro.

Phase B: Deferred. Keeping `#[sqlx(default)]` on `Task.pr_url`,
`Task.agent_type`, `Task.unresolved_blocker_count` is the lower-risk
path — removing those attributes would cascade into every runtime
`query_as::<_, Task>` list call (there are many, built with
`format!()` on top of dynamic WHERE clauses). Instead, the
`task_select_where_id!` macro and all new `query_as!` list calls
explicitly project `agent_type` plus a `CAST(0 AS SIGNED) AS
"unresolved_blocker_count!: i64"` so that `query_as!` (which ignores
`#[sqlx(default)]`) can still bind every field. Runtime paths that
don't project those columns keep working through the `sqlx(default)`
attributes as before.

Phase C (files touched):

| File | Converted this batch | Annotated runtime | Notes |
|------|---------------------:|------------------:|-------|
| `repositories/note/crud.rs`   | 18 (7 via macro + 10 static + 1 delete) | 0 | File now free of runtime-typed `sqlx::query(...)` calls. |
| `repositories/note/indexing.rs` | 2 (via macro) | 0 (on top of batch 2) | The two `NOTE_SELECT_WHERE_ID` runtime callers that were deferred in batch 2 are now macro-typed. |
| `repositories/note/consolidation.rs` | 1 (via macro) | — | Only the `NOTE_SELECT_WHERE_ID` call site here; rest of file untouched this batch. |
| `repositories/note/tests/crud_storage.rs` | 2 (via macro) | — | Completes the deferred items from batch 2. |
| `repositories/task/mod.rs`    | 3 (via macro) | — | Previously-runtime `TASK_SELECT_WHERE_ID` callers in tests + `maybe_reopen_epic` fixtures. |
| `repositories/task/reads.rs`  | 7 (6 static Task list + 1 macro + 1 scalar) | 2 (format!()-built list/export) | Task SELECTs now explicitly project `agent_type` + `CAST(0 AS SIGNED) AS "unresolved_blocker_count!: i64"`. |
| `repositories/task/status.rs` | 13 (7 macro + 6 static) | 1 (format!() closed_at fragment) | Full transition/update/insert path compile-checked. |
| `repositories/task/writes.rs` | 19 (8 macro + 11 static)   | 0 | Entire file free of runtime `sqlx::query(...)`. |
| `repositories/task/queries.rs`| 7 (2 macro + 5 static)     | 6 (format!()-built list/count + 2 `Row::get()` aggregations in `board_health`) | Static claim/reconcile paths macro-typed. |

Batch-3 new compile-time checked call sites: **~41** on top of Phase-A
macro refactor touching **~40** additional call sites.

**Converted total (cumulative) end of batch 3**: **~158** out of ~355 queries.
**Annotated-as-dynamic (cumulative)**: **~60**.
**`.sqlx/` cache**: 144 entries (from 118 at end of batch 2, +26 new).

## Done (fully or partially converted to macros)

### Batch 1

| File | Converted | Deferred | Notes |
|------|-----------|----------|-------|
| `repositories/session_auth.rs` | 5 | 0 | All static queries macro-ified. `const COLS` removed. |
| `repositories/repo_graph_cache.rs` | 2 | 0 | — |
| `repositories/repo_map_cache.rs` | 3 | 0 | Uses `worktree_path AS "worktree_path: Option<String>"` override. |
| `repositories/verification_cache.rs` | 5 | 0 | `duration_ms AS "duration_ms!: i64"` override. |
| `repositories/verification_result.rs` | 5 | 0 | — |
| `repositories/settings.rs` | 5 | 0 | `key` and `value` are reserved — aliased as `\`key\` AS \`key\``. |
| `repositories/git_settings.rs` | 4 | 0 | — |
| `repositories/task/activity.rs` | 5 | 1 | `query_activity` uses `format!()` WHERE clause — annotated. |
| `repositories/session_message.rs` | 6 | 1 | `load_for_sessions` builds IN-list at runtime — annotated. |
| `repositories/dolt_branch.rs` | 0 | 2 | All queries hit Dolt-only `dolt_branches` / SQL param — annotated. |
| `repositories/dolt_history_maintenance.rs` | 0 | 4 | All against `dolt_log` / `information_schema` / runtime table names — annotated. |

### Batch 2

| File | Converted | Deferred | Notes |
|------|-----------|----------|-------|
| `repositories/note/housekeeping.rs` | 5 | 0 | `abstract` needs backticks (`` `abstract` AS abstract_ ``). Dropped unused selects from `BrokenLinkCandidateRow`. |
| `repositories/note/context.rs` | 3 | 3 | `fts_candidates` dispatches across pool kinds; `fetch_l0/l1_notes` build runtime IN-lists — annotated. |
| `repositories/note/tests/session_scoped_consolidation.rs` | 1 | 0 | Test-only; converted positional `?1..?3` → `?` placeholders. |
| `repositories/note/tests/wikilink_graph.rs` | 2 | 0 | Same positional → `?` rewrite. |
| `repositories/note/tests.rs` | 0 | 4 | Helpers use SQLite-only `pragma_table_info` and `strftime` — annotated. |
| `repositories/note/tests/crud_storage.rs` | 2 | 2 | `query_as::<_, Note>(NOTE_SELECT_WHERE_ID)` can't go through `query_as!` (const is not a literal). Annotated. |
| `repositories/note/tests/search_ranking.rs` | 6 | 0 | All simple scalar + update. |
| `repositories/test_support.rs` | 6 | 0 | `Project.auto_merge/sync_enabled` needed `AS "…!: bool"` nullability/type override. |
| `repositories/note/indexing.rs` | 6 | 2 | Two `query_as::<_, Note>(super::NOTE_SELECT_WHERE_ID)` callers kept runtime — same `const`-literal issue. |
| `repositories/note/scoring.rs` | 6 | 2 | `note_confidence_map`, `temporal_scores` stay dynamic (IN list) — annotated. |
| `repositories/task/blockers.rs` | 7 | 1 | `emit_unblocked_tasks` selects into `Task` which has `#[sqlx(default)]` fields → kept runtime. Cycle-detect uses `SELECT EXISTS(...) AS "exists!: i64"` override. `BlockerRef` macro-typed cleanly. |
| `repositories/note/tests/consolidation_housekeeping.rs` | 9 | 1 | Positional `?N` → `?`. `abstract` backticked. One helper still uses SQLite `datetime('now','-31 days')` — annotated. |
| `repositories/task/mod.rs` | 5 | 0 | Test fixtures + `maybe_reopen_epic`. `Epic.auto_breakdown` needs `AS "…!: bool"` override. |
| `repositories/note/embeddings.rs` | 12 | 3 | LEFT JOIN nullability overrides (`AS "…: Option<String>"`) needed for `EmbeddingRepairRow`. SQLite-vec helpers (`note_embeddings_vec`) annotated. |
| `repositories/task/reads.rs` | 2 | 24 | Only the scalar `COUNT(*) FROM epics` helpers converted; the large `Task`-returning queries and dynamic WHERE clauses stay runtime. |

**Converted total (cumulative)**: **~117** `query!` / `query_as!` / `query_scalar!` calls.
**Annotated-as-dynamic total (cumulative)**: **~50** (dolt-only, SQLite-only, runtime-built SQL, or shared-const SELECT).
**`.sqlx/` cache**: regenerated three times during batch 2; all new entries committed.

## Remaining (runtime-typed, NOT yet converted)

All files below are either fully converted or annotated as dynamic. The
~45 annotated-runtime sites are either:
- `format!()`-built SQL with variable WHERE/IN/ORDER clauses, or
- SQLite-only test fixtures using `datetime('now')`, `strftime`, positional `?N`.

No further mechanical conversion is possible without either introducing
runtime-dispatched SQL or splitting the annotated dynamic queries into
fixed variants.

Grand total remaining runtime-typed call-sites: **~45** (all annotated).

## Known blocker: shared `SELECT` constants vs. `query_as!`

`sqlx::query_as!` requires a **string literal** for its SQL argument. The
repo uses two shared `const &str` projections, `NOTE_SELECT_WHERE_ID` and
`TASK_SELECT_WHERE_ID`, that are referenced from a dozen call sites each
(`note/crud.rs`, `note/indexing.rs`, `note/consolidation.rs`,
`note/tests/crud_storage.rs`, `task/reads.rs`, `task/status.rs`,
`task/writes.rs`, `task/queries.rs`). Converting them to the macro form
requires either:

1. Replacing the const with a `macro_rules! note_select_where_id { () => { "…" } }`
   and updating every call site to use `sqlx::query_as!(Note, note_select_where_id!(), id)` —
   note that `query_as!` **does** accept nested macro expansions (verified during
   batch 2; an earlier attempt failed and was reverted, but the failure was actually
   a type-annotation edge case that can be fixed by binding the result to a
   `let` with an explicit `Vec<Note>`/`Note` type, or calling the macro with
   `concat!("…")`).  A follow-up sub-batch should make that mechanical change.
2. Inlining the SELECT text into each call site (simpler, but 10+ copies of a
   long multi-line string).

Until one of those is done, the remaining `NOTE_SELECT_WHERE_ID` /
`TASK_SELECT_WHERE_ID` call sites must stay runtime-typed and carry the
`// NOTE: …` annotation.

Second blocker: `Task` and some other structs use `#[cfg_attr(feature = "sqlx",
sqlx(default))]` on newer columns (`pr_url`, `agent_type`,
`unresolved_blocker_count`). `query_as!` does not honour `#[sqlx(default)]`,
so every SELECT that doesn't list those columns explicitly fails to
compile as a macro. Options: add those columns to every SELECT (verbose),
or switch to `query!` + manual struct construction.

## Method for the next agent

The mechanical conversion pattern is:

```rust
// Before
sqlx::query("INSERT INTO t (a, b) VALUES (?, ?)").bind(a).bind(b).execute(pool).await?

// After
sqlx::query!("INSERT INTO t (a, b) VALUES (?, ?)", a, b).execute(pool).await?
```

```rust
// Before
sqlx::query_as::<_, Row>("SELECT …").bind(x).fetch_one(pool).await?

// After
sqlx::query_as!(Row, "SELECT …", x).fetch_one(pool).await?
```

When `query_as!` complains about nullable columns, use the inline type override:

```rust
r#"SELECT col AS "col!: i64" FROM t"#     // force non-null
r#"SELECT col AS "col: Option<i64>" FROM t"#   // force nullable
```

`format!()`-built SQL stays as `sqlx::query`/`sqlx::query_as::<_, …>` — add:
```
// NOTE: dynamic SQL — compile-time check not possible
```

After each batch:
```bash
cd server && DATABASE_URL=mysql://root@127.0.0.1:3306/djinn cargo check -p djinn-db --tests
```

After 3–5 files, regenerate the offline cache:
```bash
cd server/crates/djinn-db && DATABASE_URL=mysql://root@127.0.0.1:3306/djinn cargo sqlx prepare --workspace -- --all-targets --tests
```
(note: run it from inside `crates/djinn-db` — running from workspace root misses `#[cfg(test)]` call-sites).

Commit the `.sqlx/` delta separately.
