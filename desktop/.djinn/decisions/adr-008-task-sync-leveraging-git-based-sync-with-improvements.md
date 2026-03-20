---
title: "ADR-008: Task Sync — Leveraging Git-Based Sync with Improvements"
type: adr
tags: ["adr", "sync", "git", "server", "multi-device", "multi-project"]
---

## Context

The old CLI project (`../cli`) had a mature git-based sync system for sharing task state across machines and team members. The CLI is no longer used — the Rust server (`../server`) is now the runtime, and the desktop app is the UI.

The new server already has a partial port of the sync system in `src/sync/`, but several capabilities from the old CLI were not carried over, and the current architecture has structural issues that need addressing before multi-device and team sync can work correctly.

### Old CLI Sync — How It Worked

The CLI sync used an **orphan git branch** (`djinn/tasks`) as a transport layer:

1. **Per-user JSONL files**: Each user writes `{email}.jsonl` containing all their tasks
2. **Export**: Local SQLite -> serialize to JSONL -> commit to orphan branch -> fetch -> rebase -> push (retry x2)
3. **Import**: Two-phase pull — cheap `git ls-remote` SHA check (~50ms), full fetch only if SHA differs. Then read all `*.jsonl` files and upsert with Last-Writer-Wins on `updated_at`
4. **Auto-sync**: 30s polling loop for imports; exports triggered on task completion/pause/stop
5. **Conflict resolution**: Three layers — short ID collisions (UUIDv7 tiebreak), state conflicts (LWW), push conflicts (abort rebase -> re-export -> retry)
6. **Backoff**: Exponential 30s -> 30min on failures, `NeedsAttention` flag after 3+ consecutive failures
7. **Reconciliation**: When importing a user's file, tasks missing from it are closed locally (orphan cleanup)
8. **Data synced**: Tasks, Activities, Blockers, Memory References, Phases
9. **Terminal state protection**: Won't regress closed/approved tasks to non-terminal statuses

### What the New Server Already Has

The Rust server (`src/sync/`) already implements the core:

| Feature | Status | Location |
|---------|--------|----------|
| SyncManager with pluggable channels | Done | `sync/mod.rs` (SYNC-01) |
| Tasks channel, JSONL per-user files | Done | `sync/tasks_channel.rs` |
| Fetch-rebase-push with retry x3 | Done | `tasks_channel::export()` |
| Exponential backoff (30s -> 15min) | Done | `sync/backoff.rs` (SYNC-03) |
| Enable/disable per-machine & team-wide | Done | `SyncManager::enable/disable` (SYNC-04) |
| Channel failure isolation | Done | SYNC-05 |
| MCP tools (enable/disable/export/import/status) | Done | `mcp/tools/sync_tools.rs` |
| Auto-export on task mutations (debounced 10s) | Done | `SyncManager::spawn_background_task` |
| LWW upsert with `updated_at` guard | Done | `TaskRepository::upsert_peer` |
| Background fallback export every 5min | Done | `spawn_background_task` |

### What's Missing

| Gap | Impact | Severity |
|-----|--------|----------|
| **Import-export loop** | `upsert_peer` emits `TaskUpdated` events, triggering re-export of just-imported data. Without a guard, every import causes a pointless commit+push cycle | **Critical** |
| **Single-project limitation** | `SyncManager` stores one `project_path` per channel. Only one project can sync despite `Project.sync_enabled` existing per-project in the DB | **Critical** |
| **No auto-import** | Only exports run automatically; imports require manual MCP call | High |
| **No two-phase pull** | Every import does a full fetch even when nothing changed | High |
| **No closed-task eviction** | All tasks exported forever — file grows unboundedly | High |
| **No import atomicity** | Each `upsert_peer` is independent — crash mid-import + SHA update = permanent data loss | High |
| **No terminal state protection** | `upsert_peer` could regress a closed task back to open | Medium |
| **No peer reconciliation** | Stale tasks from removed team members persist forever | Medium |
| **No SSE sync events** | Desktop has no visibility into sync operations | Medium |
| **No short ID collision handling** | Two users could create tasks with the same short ID | Low |

## Decision

Implement sync improvements on the server in two tiers.

### Tier 1 — Core (Required for Multi-Device)

#### 1a. Import-Export Loop Guard (SYNC-06)

`upsert_peer` (in `task/reads.rs:191`) fires `DjinnEvent::TaskUpdated` on every successful upsert. `spawn_background_task` listens for `TaskUpdated` and schedules an export. This creates a loop: import -> upsert -> event -> export -> push -> SHA changes -> next import sees new SHA -> repeat.

**Fix**: Add a `from_sync: bool` field to `TaskCreated` and `TaskUpdated` events. `upsert_peer` emits with `from_sync: true`. The background export listener ignores events where `from_sync == true`.

```rust
// events.rs
TaskCreated { task: Task, from_sync: bool },
TaskUpdated { task: Task, from_sync: bool },

// spawn_background_task: only set pending on local mutations
DjinnEvent::TaskCreated { from_sync: false, .. }
| DjinnEvent::TaskUpdated { from_sync: false, .. }
| DjinnEvent::TaskDeleted { .. } => { pending = true; }
```

SSE serialization strips the `from_sync` field — the desktop doesn't need to know the origin.

#### 1b. Multi-Project Sync (SYNC-07)

Replace the single-project-per-channel model. Sync iterates over all projects where `sync_enabled = true` in the `projects` table.

**Branch layout** — per-project subdirectories:

```
djinn/tasks branch:
  {project_name}/{user_id}.jsonl
  {project_name}/{user_id}.jsonl
```

**Export**: For each project where `sync_enabled = true`, query its tasks, write to `{project.name}/{user_id}.jsonl`.

**Import**: List subdirectories on the branch. For each, check if a local project with that name exists and has `sync_enabled = true`. Skip subdirectories for projects the user doesn't have. USER1 has projects A, B, C — exports all three. USER2 has A and C — only imports A and C, never touches B.

**Migration from current layout**: The current code writes `{user_id}.jsonl` at the root. First export under the new layout writes to `{project_name}/{user_id}.jsonl` and deletes any root-level `*.jsonl` files.

Remove `SyncManager`'s per-channel `project_path` field entirely. The source of truth becomes the `projects` table.

#### 1c. Two-Phase Pull (SYNC-08)

Before doing a full fetch+import, run `git ls-remote --heads origin djinn/tasks` and compare the SHA against a persisted `last_imported_sha`.

```
settings key: sync.tasks.last_imported_sha
```

Persist in DB (not in-memory like the old CLI) so restarts don't trigger unnecessary re-imports. **Only update this SHA after the import transaction commits** (see 1e).

#### 1d. Auto-Import Loop (SYNC-09)

Add a periodic import to `spawn_background_task`. Cadence: every 60 seconds. Uses two-phase pull to make idle polls near-free (~50ms).

```rust
// In spawn_background_task, add a third interval:
let mut import_interval = tokio::time::interval(Duration::from_secs(60));
```

The import path uses `upsert_peer` which emits `from_sync: true` events (1a), so imports never trigger re-exports.

#### 1e. Import Transaction (SYNC-10)

Wrap all `upsert_peer` calls for a single import in one SQLite transaction:

```rust
let mut tx = db.pool().begin().await?;
for task in peer_tasks.into_values() {
    upsert_peer_in_tx(&mut tx, &task).await?;
}
tx.commit().await?;
// Only NOW persist last_imported_sha
```

Either all tasks land or none do. If the server crashes at task #30 of 50, the SHA is not updated, so the next import retries all 50. Without this, a partial import that updates the SHA silently loses the remaining tasks.

#### 1f. Terminal State Protection (SYNC-11)

Add a guard to `upsert_peer`: if the local task is in a terminal status (`closed`) and the incoming task is not, skip the upsert regardless of `updated_at`. A closed task should only be reopened by explicit local action.

```sql
-- Add to the ON CONFLICT WHERE clause:
AND NOT (tasks.status = 'closed' AND excluded.status != 'closed')
```

#### 1g. Closed Task Eviction (SYNC-12)

Only export non-closed tasks and tasks closed within the last 1 hour. After 1 hour, the task is dropped from the JSONL entirely.

```sql
-- Export query per project:
WHERE project_id = ?1
  AND (status != 'closed' OR closed_at > datetime('now', '-1 hour'))
```

**Why 1 hour**: Auto-import runs every 60s. A closed task will be synced to all online peers within 1-2 minutes. The 1-hour window provides generous margin for machines that are temporarily offline (e.g., laptop lid closed). Machines offline for longer than 1 hour rely on peer reconciliation (Tier 2) to catch up.

**Why not longer**: Keeping closed tasks in the export risks users seeing and accidentally interacting with them on other machines.

#### 1h. SSE Sync Events (SYNC-13)

Add a new event variant emitted after each export/import:

```rust
DjinnEvent::SyncCompleted {
    channel: String,
    direction: String,    // "export" | "import"
    count: usize,
    error: Option<String>,
}
```

The desktop uses this for a sync status indicator in the sidebar. Clicking the indicator triggers a manual export+import via the existing MCP tools.

### Tier 2 — Robustness (Required for Teams)

#### 2a. Peer Reconciliation (SYNC-14)

When importing a peer's JSONL file, build a set of all task IDs in that file. For tasks in the local DB that are:
- (a) owned by that peer's `user_id`, AND
- (b) not in the peer's file, AND
- (c) not in a terminal state (`closed`)

Close them with `close_reason: "peer_reconciled"`.

This handles two scenarios:
1. A team member deletes tasks from their board
2. Closed tasks that aged out of the 1-hour export window on the peer's machine

**Safety guard**: Only reconcile if the peer's file contains at least 1 task. An empty file (export bug, corrupted write) skips reconciliation entirely.

#### 2b. Short ID Collision Handling (SYNC-15)

On import, if an `INSERT` fails due to `UNIQUE(short_id)` constraint, extend the incoming task's `short_id` by one character (from its UUID) and retry. Log the collision for diagnostics.

#### 2c. Sync Health Banner (SYNC-16)

After 3+ consecutive failures on any channel, set a `needs_attention` flag in the channel status. The desktop shows a persistent banner: "Sync failing — check git remote configuration."

## Consequences

**Positive:**
- Multi-device sync works out of the box once a git remote is configured
- Multi-project sync requires zero configuration — if you have the project, you get its tasks
- Two-phase pull keeps idle polling near-free (~50ms network call, 95% of cycles)
- Import transactions guarantee all-or-nothing consistency
- Loop guard eliminates pointless commit+push cycles from sync-triggered events
- 1-hour closed task eviction keeps JSONL files small (only active work + recently closed)
- Peer reconciliation catches anything that falls outside the eviction window
- SSE events give the desktop real-time sync visibility without polling
- Persisted SHA survives server restarts (improvement over old CLI)
- Pluggable channel architecture supports future `djinn/memory` and `djinn/settings` channels

**Negative:**
- Git CLI dependency remains (requires `git` on PATH) — acceptable for developer tooling
- JSONL files are plaintext on the sync branch — task titles/descriptions visible to anyone with repo access
- Per-project subdirectories increase the number of files on the sync branch (one per user per project)

**Risks:**
- **Rebase conflicts**: Rare (per-user files) but possible if the same user runs two machines simultaneously. The existing retry loop handles this.
- **Reconciliation false positives**: A peer with a corrupted/empty export could trigger incorrect closures. Mitigated by the "at least 1 task" guard.
- **1-hour eviction vs long offline**: A machine offline for >1 hour misses the closed task in the JSONL. Reconciliation (2a) handles this — the task disappears from the peer's file entirely, triggering reconciliation closure. Without Tier 2, these tasks remain open locally until manually closed.

## Implementation Order

Tier 1 items have dependencies:

```
1a (loop guard) ── must be first, blocks auto-import
1b (multi-project) ── can be parallel with 1a
1c (two-phase pull) ── needs 1e (SHA must be transactional)
1d (auto-import) ── needs 1a + 1c
1e (import transaction) ── needs 1c
1f (terminal protection) ── independent, can be anytime
1g (closed task eviction) ── independent, can be anytime
1h (SSE events) ── independent, can be anytime
```

Suggested sequence: `1a -> 1e -> 1c -> 1b -> 1d -> 1f -> 1g -> 1h`

## Relations

- [[ADR-005: Project Scoped Epics, Tasks, and Sessions]]
- [[ADR-006: Desktop Uses MCP SDK Directly from Frontend]]
- [[Roadmap]]
