---
title: "Sync Improvements Implementation Plan"
type: design
tags: ["sync", "git", "multi-device", "multi-project", "testing"]
related: ["ADR-008 (desktop)", "ADR-026"]
---

# Sync Improvements — Implementation Plan

**Source ADR:** `desktop/.djinn/decisions/adr-008-task-sync-leveraging-git-based-sync-with-improvements.md`
**Test Strategy:** `server/.djinn/decisions/adr-026-automated-testing-strategy-three-phase-full-stack-coverage.md`

## Overview

8 implementation items in 2 tiers. ~30 tests. Mostly Rust server changes with one small desktop SSE listener addition.

## Dependency Graph

```
Stream A (critical path):
  SYNC-06 (loop guard) → SYNC-10 (import tx) → SYNC-08 (two-phase pull) → SYNC-09 (auto-import)

Stream B (parallel from start):
  SYNC-07 (multi-project)

Stream C (independent, parallel from start):
  SYNC-11 (terminal protection)
  SYNC-12 (closed task eviction)
  SYNC-13 (SSE sync events)
```

**Important:** SYNC-06 touches `events.rs` which SYNC-13 also touches. Coordinate or sequence 06 before 13.

---

## SYNC-06: Import-Export Loop Guard

**Priority:** Critical — blocks SYNC-09 (auto-import)
**Files:**
- `src/events.rs` — add `from_sync` field to `TaskCreated`, `TaskUpdated`
- `src/db/repositories/task/reads.rs` — `upsert_peer()` emits with `from_sync: true`
- `src/sync/mod.rs` — `spawn_background_task()` filters `from_sync == false`
- `src/sse.rs` — strip `from_sync` from SSE envelope serialization

**Implementation:**

1. Update `DjinnEvent` enum:
```rust
// events.rs
TaskCreated { task: Task, from_sync: bool },
TaskUpdated { task: Task, from_sync: bool },
```

2. All existing local task creation/update call sites emit `from_sync: false`. Grep for `DjinnEvent::TaskCreated` and `DjinnEvent::TaskUpdated` across all files and update.

3. `upsert_peer()` emits with `from_sync: true`.

4. `spawn_background_task()` match arms:
```rust
DjinnEvent::TaskCreated { from_sync: false, .. }
| DjinnEvent::TaskUpdated { from_sync: false, .. }
| DjinnEvent::TaskDeleted { .. } => { pending = true; }
```

5. SSE serialization: `from_sync` not included in the JSON envelope sent to desktop.

**Tests (4):**

| Test | Type | What it validates |
|------|------|-------------------|
| `from_sync_true_skipped_by_background_task` | Unit | Subscribe to events channel. Emit `TaskUpdated { from_sync: true }`. Verify background export NOT triggered. Use `tokio::time::pause()` + advance to check no export fires. |
| `from_sync_false_triggers_background_export` | Unit | Emit `TaskCreated { from_sync: false }`. Verify export IS triggered within debounce window. |
| `upsert_peer_emits_from_sync_true` | Unit | Call `upsert_peer()`, receive event on channel, assert `from_sync == true`. |
| `sse_envelope_excludes_from_sync` | Unit | Serialize a `TaskUpdated { from_sync: true }` to SSE envelope JSON. Assert the JSON does not contain `from_sync` key. |

---

## SYNC-10: Import Transaction

**Priority:** High — blocks SYNC-08 (SHA must only persist after successful import)
**Files:**
- `src/sync/tasks_channel.rs` — wrap import in SQLite transaction
- `src/db/repositories/task/reads.rs` — add `upsert_peer_in_tx()` variant that takes `&mut Transaction`

**Implementation:**

1. Add `upsert_peer_in_tx(&mut tx: &mut Transaction, task: &Task) -> Result<bool>` — same logic as `upsert_peer` but executes within provided transaction instead of creating its own.

2. Refactor `tasks_channel::import()`:
```rust
let mut tx = db.pool().begin().await?;
let mut count = 0;
for task in peer_tasks.into_values() {
    if upsert_peer_in_tx(&mut tx, &task).await? {
        count += 1;
    }
}
tx.commit().await?;
// Only NOW persist last_imported_sha (added in SYNC-08)
```

3. Collect events during transaction, emit AFTER commit succeeds.

**Tests (3):**

| Test | Type | What it validates |
|------|------|-------------------|
| `import_all_tasks_land_on_success` | Integration | Create 5 tasks in JSONL, import, verify all 5 in DB. |
| `import_rolls_back_on_failure` | Integration | Import 5 tasks where task #3 has invalid FK (bad epic_id). Verify tasks #1-2 also NOT in DB. |
| `events_emitted_only_after_commit` | Unit | Subscribe to event channel. Start import of 3 tasks. Verify no events received until after commit. |

---

## SYNC-08: Two-Phase Pull

**Priority:** High — makes idle polling near-free
**Depends on:** SYNC-10 (SHA persistence must be transactional)
**Files:**
- `src/sync/tasks_channel.rs` — add `git ls-remote` check before fetch
- `src/db/repositories/settings.rs` — persist `sync.tasks.last_imported_sha`

**Implementation:**

1. Before `git fetch` in import path, run:
```rust
let remote_sha = git_ls_remote_sha(&repo_path, "djinn/tasks").await?;
let stored_sha = settings.get("sync.tasks.last_imported_sha").await?;
if Some(&remote_sha) == stored_sha.as_ref() {
    return Ok(0); // Nothing changed
}
```

2. After successful import + transaction commit:
```rust
settings.set("sync.tasks.last_imported_sha", &remote_sha).await?;
```

3. SHA persisted in DB settings table (survives server restarts — improvement over old CLI which used in-memory).

**Tests (3):**

| Test | Type | What it validates |
|------|------|-------------------|
| `skips_fetch_when_sha_unchanged` | Integration | Set up real git repo. Export once, record SHA. Call import again — verify git fetch NOT executed (check by counting git operations or mocking). |
| `fetches_when_sha_differs` | Integration | Export from user A, push. Change SHA on remote (push from user B). Import from user A — verify full fetch + import runs. |
| `sha_persisted_in_settings_table` | Unit | Write SHA to settings, create new SyncManager instance, verify it reads the SHA back correctly. |

---

## SYNC-07: Multi-Project Sync

**Priority:** Critical — required for multi-project support
**Can run in parallel with Stream A**
**Files:**
- `src/sync/mod.rs` — remove per-channel `project_path`, iterate `projects` table
- `src/sync/tasks_channel.rs` — `{project_name}/{user_id}.jsonl` layout, migration logic
- `src/db/repositories/project.rs` — query for `sync_enabled = true` projects

**Implementation:**

1. Remove `project_path: Option<PathBuf>` from `ChannelState`.

2. Export path:
```rust
let projects = project_repo.list_sync_enabled().await?;
for project in projects {
    let tasks = task_repo.list_for_export(&project.id).await?;
    write_jsonl(&worktree, &format!("{}/{}.jsonl", project.name, user_id), &tasks)?;
}
```

3. Import path:
```rust
let subdirs = list_subdirectories(&worktree)?;
for subdir in subdirs {
    let project = project_repo.find_by_name(&subdir).await?;
    if project.is_none() || !project.unwrap().sync_enabled { continue; }
    // Import tasks from {subdir}/*.jsonl
}
```

4. Migration: On first export under new layout, check for root-level `*.jsonl` files. Move contents to `{project_name}/{user_id}.jsonl`. Delete root files.

5. Update `SyncManager::enable()` / `disable()` to work per-project via the `projects` table instead of in-memory state.

6. Per-project SHA tracking: `sync.tasks.{project_name}.last_imported_sha`

**Tests (5):**

| Test | Type | What it validates |
|------|------|-------------------|
| `exports_to_project_subdirectory` | Integration | Create project "myapp", add tasks, export. Verify JSONL at `myapp/{user_id}.jsonl` on branch. |
| `imports_only_enabled_projects` | Integration | Push 3 project dirs to branch. Only 2 exist locally with `sync_enabled`. Verify third project's tasks ignored. |
| `migrates_root_level_jsonl` | Integration | Create old-format root `{user_id}.jsonl`. Run export. Verify file moved to `{project_name}/` subdir and root cleaned. |
| `project_disable_excludes_from_export` | Unit | 3 projects, disable one. Export. Verify disabled project absent from JSONL output. |
| `projects_are_independent` | Integration | 2 projects each with tasks. Export + import. Verify no cross-contamination (project A tasks only in project A). |

---

## SYNC-09: Auto-Import Loop

**Priority:** High — completes the sync loop
**Depends on:** SYNC-06 (loop guard) + SYNC-08 (two-phase pull)
**Files:**
- `src/sync/mod.rs` — add import interval to `spawn_background_task()`

**Implementation:**

Add third interval to `spawn_background_task`:
```rust
let mut import_interval = tokio::time::interval(Duration::from_secs(60));

// In select! loop:
_ = import_interval.tick() => {
    if let Err(e) = sync.import_all(&db, &user_id).await {
        tracing::warn!("auto-import failed: {e}");
    }
}
```

Two-phase pull (SYNC-08) ensures 95% of these cycles are ~50ms `ls-remote` calls.

**Tests (2):**

| Test | Type | What it validates |
|------|------|-------------------|
| `auto_import_runs_on_interval` | Integration | Use `tokio::time::pause()`. Spawn background task. Push changes to remote. Advance time by 60s. Verify import executed (tasks appear in DB). |
| `auto_import_does_not_trigger_re_export` | Integration | Push remote changes. Let auto-import run. Verify no export triggered (SYNC-06 loop guard working). Advance time past debounce window, confirm no export. |

---

## SYNC-11: Terminal State Protection

**Priority:** Medium — prevents data regression
**Independent — can be done anytime**
**Files:**
- `src/db/repositories/task/reads.rs` — guard in `upsert_peer` / `upsert_peer_in_tx`

**Implementation:**

Add to the `ON CONFLICT` WHERE clause:
```sql
AND NOT (tasks.status = 'closed' AND excluded.status != 'closed')
```

A closed task can only be updated by a peer if the peer's version is also closed (e.g., updated metadata on a closed task).

**Tests (3):**

| Test | Type | What it validates |
|------|------|-------------------|
| `closed_task_not_regressed_by_peer` | Unit | Close task locally. Import peer version with `in_progress` and later `updated_at`. Verify task stays `closed`. |
| `closed_task_updated_by_peer_close` | Unit | Close task locally. Import peer version that's also `closed` with later `updated_at`. Verify metadata updates (e.g., close reason). |
| `non_terminal_task_lww_works_normally` | Unit | Create open task. Import peer version with `in_progress` and later `updated_at`. Verify status changes to `in_progress`. |

---

## SYNC-12: Closed Task Eviction

**Priority:** Medium — prevents unbounded JSONL growth
**Independent — can be done anytime**
**Files:**
- `src/sync/tasks_channel.rs` — modify export query
- `src/db/repositories/task/reads.rs` or `writes.rs` — add `list_for_export()` query

**Implementation:**

Export query per project:
```sql
SELECT * FROM tasks
WHERE project_id = ?1
  AND (status != 'closed' OR closed_at > datetime('now', '-1 hour'))
```

**Tests (3):**

| Test | Type | What it validates |
|------|------|-------------------|
| `recently_closed_included_in_export` | Unit | Close task, export immediately. Verify task present in JSONL. |
| `old_closed_excluded_from_export` | Unit | Close task, set `closed_at` to 2 hours ago. Export. Verify task absent from JSONL. |
| `open_tasks_always_exported` | Unit | Create tasks in various statuses (open, in_progress, needs_review). Export. Verify all present. |

---

## SYNC-13: SSE Sync Events

**Priority:** Medium — desktop sync visibility
**Independent but coordinate with SYNC-06 (both touch events.rs)**
**Files:**
- `src/events.rs` — add `SyncCompleted` variant
- `src/sync/mod.rs` or `tasks_channel.rs` — emit after export/import
- `src/sse.rs` — serialize `SyncCompleted` to SSE envelope
- `desktop/src/` — listen for sync events (small TypeScript change)

**Implementation:**

1. New event variant:
```rust
DjinnEvent::SyncCompleted {
    channel: String,      // "tasks"
    direction: String,    // "export" | "import"
    count: usize,         // tasks synced
    error: Option<String>,
}
```

2. Emit at end of `export_all()` and `import_all()`.

3. SSE envelope:
```json
{ "type": "sync", "action": "completed", "data": { "channel": "tasks", "direction": "export", "count": 5, "error": null } }
```

4. Desktop: Add sync event handler in SSE listener. Update a Zustand store with last sync status for sidebar indicator.

**Tests (3):**

| Test | Type | What it validates |
|------|------|-------------------|
| `sync_completed_emitted_after_export` | Unit | Run export. Subscribe to events. Verify `SyncCompleted { direction: "export", count: N }` received. |
| `sync_completed_emitted_after_import` | Unit | Run import. Verify `SyncCompleted { direction: "import", count: N }` received. |
| `sync_error_populates_error_field` | Unit | Force export failure (e.g., no git remote). Verify `SyncCompleted { error: Some("...") }` emitted. |

---

## Integration / End-to-End Tests

These test the full sync system working together after all items are implemented.

**Files:** `src/sync/tests.rs` (new dedicated test file)

| Test | Type | What it validates |
|------|------|-------------------|
| `full_round_trip_export_import` | E2E | Create tasks locally. Export to real git repo (TempDir). Fresh DB imports from same repo. Verify all tasks match. |
| `two_users_concurrent_sync` | E2E | Two TempDir local repos sharing one bare remote. User A exports, User B exports. Both import. Verify both have all tasks. |
| `server_restart_resumes_sync` | E2E | Export, drop SyncManager, create new one from same DB. Verify SHA, enabled state, project list all restored. No unnecessary re-import. |
| `concurrent_export_and_import` | E2E | Spawn export and import simultaneously. Verify no deadlock, no data corruption, both complete. |

---

## Test Summary

| SYNC Item | Tests | Type |
|-----------|-------|------|
| SYNC-06 Loop Guard | 4 | Unit |
| SYNC-10 Import Transaction | 3 | Unit + Integration |
| SYNC-08 Two-Phase Pull | 3 | Unit + Integration |
| SYNC-07 Multi-Project | 5 | Unit + Integration |
| SYNC-09 Auto-Import | 2 | Integration |
| SYNC-11 Terminal Protection | 3 | Unit |
| SYNC-12 Eviction | 3 | Unit |
| SYNC-13 SSE Events | 3 | Unit |
| Integration / E2E | 4 | E2E |
| **Total** | **30** | |

---

## Implementation Sequence

**Phase 1 — Foundation (do first, unblocks everything):**
1. SYNC-06 (loop guard) — touches events.rs, must land before other event changes

**Phase 2 — Parallel execution after SYNC-06:**
- Stream A: SYNC-10 (import tx) → SYNC-08 (two-phase pull) → SYNC-09 (auto-import)
- Stream B: SYNC-07 (multi-project)
- Stream C: SYNC-11 + SYNC-12 + SYNC-13

**Phase 3 — Integration tests after all items land**

**Desktop change (small, after SYNC-13):**
- Add SSE sync event listener
- Add sync status to Zustand store
- Sidebar sync indicator

---

## File Change Summary

| File | Changes |
|------|---------|
| `src/events.rs` | Add `from_sync` to TaskCreated/Updated, add SyncCompleted variant |
| `src/db/repositories/task/reads.rs` | `upsert_peer_in_tx()`, terminal state guard, `from_sync: true` emission |
| `src/sync/mod.rs` | Remove per-channel project_path, multi-project iteration, auto-import interval, filter from_sync events |
| `src/sync/tasks_channel.rs` | Transaction wrapping, two-phase pull, project subdirectory layout, migration, eviction query, emit SyncCompleted |
| `src/sync/backoff.rs` | No changes |
| `src/sse.rs` | Strip `from_sync`, serialize SyncCompleted |
| `src/mcp/tools/sync_tools.rs` | Update enable/disable for per-project model |
| `src/db/repositories/project.rs` | Add `list_sync_enabled()` query |
| `src/db/repositories/settings.rs` | SHA persistence (existing set/get, no changes needed) |
| `src/server/state/mod.rs` | Update initialization for multi-project restore |
| `src/sync/tests.rs` | NEW — integration/E2E sync tests |
| `desktop/src/stores/` | Sync status store (small) |
| `desktop/src/components/Sidebar.tsx` | Sync indicator (small) |

## Relations

- [[ADR-008 (desktop)]] — source design document
- [[ADR-026]] — test strategy this plan follows
- [[ADR-019]] — MCP contract (sync tools are part of the contract)
