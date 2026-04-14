---
title: Embedded Database Survey 2026
type: research
tags: []
---

# Embedded Database Survey for Djinn Server (March 2026)

## Context

Djinn's desktop app needs to reflect server state with 100% accuracy. The current Go implementation uses a CDC pipeline (SQLite triggers -> change tail goroutine -> SSE -> MCP re-fetch) that is complex, brittle, and keeps going out of sync.

**Requirements:**
- Single central DB at `~/.djinn/` (not per-project)
- Desktop reads directly (read-only view)
- Server writes via MCP
- Change notification so desktop knows when to re-read
- Future: vector search for knowledge base / RAG
- Cross-platform: Linux, macOS, Windows (via WSL)
- Single user, local-first

**Key constraint relaxed:** Windows runs via WSL, and separate build pipelines exist in `../djinn`. CGO is now acceptable. This reopens libSQL/Turso which was previously rejected.

---

## Database Alternatives Evaluated

### DuckDB — REJECTED

- **Why:** Writer blocks ALL readers cross-process. Desktop cannot read while server writes.
- OLAP/columnar focus — slower than SQLite for row-level CRUD
- Go bindings carry "WORK IN PROGRESS" warning
- Requires CGO
- **Dealbreaker:** Concurrent access model incompatible with architecture

### libSQL / Turso — VIABLE (with Rust)

- **Go SDK (go-libsql):** Still v0.0.0 as of March 2026. Pre-release, no Windows support. Second-class FFI wrapper.
- **Rust SDK:** First-class (libSQL is written in Rust). Embedded mode works natively. DiskANN vector search available in embedded Rust mode.
- Embedded replicas: polling-based sync, not push
- CGO required for Go; native for Rust
- **Verdict:** Compelling if server is written in Rust. Non-viable in Go due to binding immaturity.

### CouchDB + PouchDB — REJECTED

- Requires abandoning Go + SQLite entirely
- Erlang server dependency
- Wrong architecture for local-first single-user

### Electric SQL — REJECTED

- Requires Postgres as source database (not SQLite)

### PowerSync — REJECTED

- Requires Postgres backend

### CR-SQLite — REJECTED

- No Go bindings; development has slowed
- Overkill for single-writer scenario

### Litestream — REJECTED

- Backup/DR tool only, not a sync engine

### LiteFS — REJECTED

- Fly.io FUSE-based, not suitable for desktop apps

### rqlite / Marmot — REJECTED

- Distributed server clusters — massive overkill for single-machine use

### SurrealDB — REJECTED

- Go SDK is a network client, not true embedded
- Requires separate process
- BSL license

### Embedded PostgreSQL — REJECTED

- Downloads 30-80MB binary on first run
- ~50MB RAM at idle (postmaster process tree)
- Overkill for hundreds-to-thousands of records
- LISTEN/NOTIFY is excellent for change detection, but the weight is unjustifiable

### PocketBase — REJECTED

- Full backend framework (auth, file storage, admin UI) just to get SQLite + realtime
- Carries heavy overhead for a library use case

### ObjectBox — REJECTED

- No SQL, no vector search in Go, unclear change notification

### Realm / MongoDB Embedded — REJECTED

- Realm deprecated (shutdown Sept 2025). No Go SDK.

---

## SQLite (Current) — Viable but Plumbing-Heavy

**Current stack:** modernc.org/sqlite (pure Go, zero CGO)

**What works:**
- Battle-tested, zero-config, file-based
- WAL mode for concurrent readers + 1 writer

**What doesn't work:**
- Cross-process change notification requires building CDC plumbing (triggers, tail goroutine, SSE, re-fetch)
- This plumbing is the core pain point — it's complex, brittle, and keeps going out of sync

**Simplest improvement if staying on SQLite:**
- Desktop opens the SQLite file directly (read-only, WAL mode)
- `PRAGMA data_version` polling every 200-500ms for change detection
- Eliminates entire CDC pipeline

**Vector search path:** sqlite-vec v0.1.0 (stable), works with both CGO and WASM drivers

---

## Recommendation

### If staying Go: SQLite with direct file access

- Desktop opens `~/.djinn/board.db` read-only in WAL mode
- `PRAGMA data_version` polling replaces entire CDC pipeline
- sqlite-vec for future vector search
- Minimal change from current architecture

### If moving to Rust: libSQL (native)

- First-class Rust SDK (libSQL is Rust)
- DiskANN vector search built-in for embedded mode
- Desktop can open the same file read-only
- Rust compiler prevents the classes of bugs that plague AI-generated Go code (see ADR-001)
- Axum + Tokio for HTTP/MCP server

---

## Complementary Options

### chromem-go / Rust equivalent

Pure in-memory vector store with file persistence. Can run alongside SQLite/libSQL as a dedicated vector index without adding a second database process. Useful if sqlite-vec or DiskANN proves insufficient.

---

## Sources

- [libSQL / Turso GitHub](https://github.com/tursodatabase/libsql) — 16k+ stars
- [go-libsql](https://github.com/tursodatabase/go-libsql) — v0.0.0, pre-release
- [sqlite-vec](https://alexgarcia.xyz/sqlite-vec/go.html) — v0.1.0, stable
- [ncruces/go-sqlite3](https://github.com/ncruces/go-sqlite3) — v0.30.5, pure Go via WASM
- [DuckDB concurrency docs](https://duckdb.org/docs/stable/connect/concurrency) — single writer XOR multiple readers
- [embedded-postgres-go](https://github.com/fergusstrange/embedded-postgres) — v1.33.0
- [SQLite Forum: cross-process change notification](https://sqlite.org/forum/info/d2586c18e7197c39c9a9ce7c6c411507c3d1e786a2c4889f996605b236fec1b7)
