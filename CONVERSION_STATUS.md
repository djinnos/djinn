# SQLx compile-time conversion status

Branch: `feat/sqlx-compile-time-convert`

## Done (fully or partially converted to macros)

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

**Converted total**: **45** `query!` / `query_as!` / `query_scalar!` calls (9 of them inside `#[cfg(test)]`).
**Annotated-as-dynamic total**: **8** (dolt-only or runtime-built SQL).
**`.sqlx/` cache entries committed**: **44**.

## Remaining (runtime-typed, NOT yet converted)

Ordered by query count, smallest first (recommended traversal order):

| File | Queries (approx) | Status |
|------|------------------|--------|
| `repositories/note/housekeeping.rs` | 5 | untouched |
| `repositories/note/context.rs` | 6 | untouched |
| `repositories/note/tests/session_scoped_consolidation.rs` | 1 | untouched (test) |
| `repositories/note/tests/wikilink_graph.rs` | 2 | untouched (test) |
| `repositories/note/tests.rs` | 4 | untouched (test) |
| `repositories/note/tests/crud_storage.rs` | 4 | untouched (test) |
| `repositories/note/search.rs` | 17 (2 remain runtime — dynamic IN) | untouched |
| `repositories/note/tests/search_ranking.rs` | 6 | untouched (test) |
| `repositories/test_support.rs` | 6 | untouched (test infra) |
| `repositories/note/indexing.rs` | 8 | untouched |
| `repositories/note/scoring.rs` | 8 | untouched |
| `repositories/task/blockers.rs` | 9 | untouched |
| `repositories/note/tests/consolidation_housekeeping.rs` | 10 | untouched (test) |
| `repositories/task/mod.rs` | 10 | untouched |
| `repositories/note/crud.rs` | 24 (6 converted, 18 remain) | partial — was touched on main infra prep, still has runtime queries |
| `repositories/task/status.rs` | 14 | untouched |
| `repositories/note/graph.rs` | 16 | untouched |
| `repositories/note/tests/graph_scoring.rs` | 16 | untouched (test) |
| `repositories/task/reads.rs` | 16 | untouched |
| `repositories/note/consolidation.rs` | 17 | untouched |
| `repositories/note/embeddings.rs` | 17 | untouched |
| `repositories/task/queries.rs` | 17 | untouched |
| `repositories/task/writes.rs` | 19 | untouched |
| `repositories/note/association.rs` | 23 | untouched |
| `repositories/agent.rs` | 25 | untouched |
| `repositories/session.rs` | 27 | untouched |
| `repositories/project.rs` | 28 (4 converted, 24 remain) | partial — project.rs was touched on main earlier, still has runtime queries |
| `repositories/epic.rs` | 29 | untouched |

Grand total remaining: ~**310 runtime-typed query call-sites** (many are multi-line; the spec figure of 259 refers to *logical* queries, not `sqlx::query*` text matches).

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
