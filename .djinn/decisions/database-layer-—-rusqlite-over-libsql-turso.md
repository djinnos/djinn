---
tags:
    - adr
    - database
    - sqlite
    - architecture
title: Database Layer — rusqlite over libsql/Turso
type: adr
---
# ADR-002: Database Layer — rusqlite over libsql/Turso

## Status: Accepted

## Context

The original plan called for libsql/Turso as the database layer, with Turso embedded replicas providing the desktop zero-latency local reads without CDC plumbing. Research uncovered critical problems:

1. **Turso embedded replicas are unsafe for cross-process access** — the custom WAL implementation corrupts data if a second process opens the file during sync
2. **Embedded replicas are marked "legacy"** — Turso docs recommend "Turso Sync" for new projects
3. **Turso Sync requires Turso Cloud** — cannot operate in local-only mode
4. **sqld sidecar** was the proposed workaround — adds operational complexity (process lifecycle, crash handling, port management)
5. **`Builder::new_local()` uses libsql's custom WAL** — no documented multi-process safety guarantees

Meanwhile, **standard SQLite WAL mode** (via rusqlite) has supported safe cross-process access for over a decade: one writer, many readers, snapshot isolation, zero coordination code.

## Decision

Use `rusqlite` with bundled SQLite in WAL mode instead of the `libsql` crate.

```toml
rusqlite = { version = "0.38", features = ["bundled"] }
```

### Architecture

**Desktop read architecture**: Repository-emitted events + SSE — structurally impossible to miss a change.

```
Repository.update_task(task)
  → write to DB (rusqlite)
  → broadcast::send(TaskEvent::Updated(task))     // full entity, automatic
  → SSE stream to desktop                          // full entity delivered
  → Desktop updates UI directly                    // no follow-up read needed
```

### Repository Pattern (bulletproof event emission)

All DB writes go through a `Repository` struct. The rusqlite `Connection` is a private field — nothing outside the repo can write to the DB. Every write method emits a full-entity event via `tokio::sync::broadcast`. This is structural, not manual: you literally cannot write without emitting an event because the write method includes both operations.

```rust
pub struct TaskRepository {
    conn: Arc<Mutex<Connection>>,           // PRIVATE
    events: broadcast::Sender<DbEvent>,     // PRIVATE
}

impl TaskRepository {
    pub fn update_task(&self, task: &Task) -> Result<Task> {
        let conn = self.conn.lock();
        conn.execute("UPDATE tasks SET ...", ...)?;
        let _ = self.events.send(DbEvent::Task(TaskEvent::Updated(task.clone())));
        Ok(task.clone())
    }
}
```

### Desktop Data Path

- **Real-time updates**: SSE (or WebSocket) stream from server → desktop. Events carry full entities. Desktop updates UI directly from event payload — no follow-up DB read or MCP tool call needed.
- **Initial load / reconnect**: Desktop reads DB file directly (rusqlite read-only, WAL mode) for local mode. Falls back to MCP tool reads for VPS mode.
- **Mutations**: Desktop calls MCP tools on the server (task_create, memory_write, etc.). Server is the single writer and enforces business logic.

### Why this is bulletproof

1. **Structural guarantee**: The `Connection` is private to the `Repository`. Rust's visibility rules make it compile-time impossible to bypass.
2. **Full entity in events**: The repo method already has the data — no extra read, no serialization mismatch.
3. **SSE auto-reconnect**: Browser EventSource reconnects automatically. On reconnect, desktop re-fetches visible data from DB file (local) or MCP tools (VPS).
4. **Optional safety net**: `rusqlite::update_hook` (feature: `hooks`) can assert in dev mode that no write bypasses the repository.

### Works uniformly across deployment modes

| Mode | SSE events | Initial load / reconnect | Mutations |
|---|---|---|---|
| Local | localhost SSE | Direct DB file read | MCP tools |
| WSL | TCP SSE (crosses boundary) | Direct DB file or MCP tools | MCP tools |
| VPS (v2+) | Remote SSE | MCP tools | MCP tools |

This completely eliminates:
- The Go server's brittle CDC pipeline (triggers → change tail → SSE → re-fetch)
- The "forgot to dispatch an event" class of bugs (structural guarantee, not discipline)
- Stale data from missed events (full entity in every event)
- The need for Turso replicas, sqld sidecar, or any sync mechanism

**Local mode** (server + desktop on same machine):
- Server opens `~/.djinn/djinn.db` with read+write, WAL enabled
- Desktop opens `~/.djinn/djinn.db` with `SQLITE_OPEN_READ_ONLY`
- Events via MCP connection, reads via direct file access
- No sidecar, no HTTP for reads, no sync mechanism, no cloud dependency

**WSL mode** (server in WSL, desktop on Windows):
- Attempt direct file access via `\\wsl$\` path
- If SQLite `-shm` shared memory doesn't work across 9P boundary, desktop falls back to reading via MCP tools (already needed for mutations)
- Events still via MCP connection regardless
- Runtime detection, not new infrastructure

**VPS mode** (v2+, out of scope for v1):
- Desktop reads via MCP tools over HTTP (no local file access)
- Events via MCP connection over the same HTTP transport

### Connection discipline

**Writer (server):**
- Single `Connection` behind a `Mutex` or dedicated write thread with channel
- `PRAGMA journal_mode=WAL` (set once at DB creation)
- `PRAGMA busy_timeout=5000`
- `PRAGMA synchronous=NORMAL`
- `PRAGMA foreign_keys=ON`
- `PRAGMA cache_size=-64000` (64 MB)
- All write transactions use `BEGIN IMMEDIATE`
- Periodic `PRAGMA wal_checkpoint(PASSIVE)` (~30s background timer)

**Reader (desktop):**
- Open with `SQLITE_OPEN_READ_ONLY`
- `PRAGMA busy_timeout=5000`
- Multiple read connections are safe

### FTS5
Included in rusqlite's bundled SQLite build (`SQLITE_ENABLE_FTS5`). Works in WAL mode with no special configuration.

### Vector search (v2)
Use `sqlite-vec` extension (asg017/sqlite-vec) loaded at runtime via `Connection::load_extension()`. Lightweight, pure C, no Faiss dependency.

## Consequences

### Positive
- **Zero cross-process plumbing** — desktop just opens the file, reads it
- **No cloud dependency** — works fully offline, no Turso account needed
- **No sidecar process** — eliminates sqld lifecycle management
- **Battle-tested** — SQLite WAL multi-process access is the most proven database pattern in existence
- **Simpler dependency** — rusqlite is a thin wrapper, not an opinionated SDK

### Negative
- **No built-in replication** — VPS mode (v2+) needs a different approach for desktop reads (HTTP/MCP tools)
- **No DiskANN vector search** — must use sqlite-vec extension instead (adequate for v1 scope where vector search is out anyway)
- **Async story** — rusqlite is sync; writes must use `spawn_blocking` in Tokio. This is actually safer than libsql's async API for SQLite's synchronous locking model.
- **WSL uncertainty** — SQLite `-shm` over `\\wsl$\` 9P is untested; may need runtime fallback

### Supersedes
- [[Embedded Database Survey]] — original decision to use libsql/Turso
- Stack Research section on Turso patterns — those API examples are now irrelevant

## Relations
- [[Project Brief]] — updates database constraint
- [[Embedded Database Survey]] — superseded by this ADR
- [[Stack Research]] — libsql patterns replaced by rusqlite
- [[Architecture Research]] — sqld sidecar architecture no longer needed
- [[Pitfalls Research]] — Turso cross-process pitfall resolved by avoiding Turso entirely


## Amendment — rusqlite version pin (2026-03-03)

### Context

Adding the `goose` crate as an in-process agent harness (ADR-008) introduced a `libsqlite3-sys` conflict:

- `rusqlite 0.37` requires `libsqlite3-sys ^0.35.0`
- `goose`'s `sqlx 0.8.x` requires `libsqlite3-sys ^0.30.1`
- Cargo's `links = "sqlite3"` constraint forbids two versions of `libsqlite3-sys` in one binary

### Resolution

`rusqlite` is pinned at `=0.32.1` in Cargo.toml. This version uses `libsqlite3-sys ^0.30.1` — the same range as sqlx-sqlite 0.8.1+, allowing both to resolve to a single `0.30.x` version.

```toml
# Pinned to 0.32.1: shares libsqlite3-sys ^0.30.1 with goose's sqlx (0.8.1+).
rusqlite = { version = "=0.32.1", features = ["bundled", "hooks"] }
```

The version number shown in the Decision section (`0.38`) was aspirational; the effective version is `0.32.1`. All DB-layer API we use (`Connection`, `params!`, `types::Value`, `hooks::Action`, `ErrorCode`) is identical between 0.32 and 0.37.

### Future path

A tracked task exists to migrate the DB layer from rusqlite to sqlx entirely. This eliminates the pin, gives async-native queries, and unifies the sqlite library with what goose already uses. See task "Migrate DB layer from rusqlite to sqlx" (label: tech-debt, database).