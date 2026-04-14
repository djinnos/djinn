---
title: Stack Research
type: research
tags: []
---

# Rust MCP Server Stack Deep Dive (March 2026)

## Core Stack Versions

| Crate | Version | Notes |
|---|---|---|
| axum | 0.8.8 | Stable, released Jan 2025. Do NOT use 0.7. |
| libsql | 0.9.29 | Use `features = ["replication"]` for embedded replicas. Requires C compiler + CMake. |
| rmcp | 0.16.0 | Official MCP SDK. Verify if 0.17.0 exists. Features: `server`, `macros`, `transport-streamable-http-server` |
| tokio | 1.x | Full features. Use `tokio-util` for CancellationToken + TaskTracker |
| serde | 1.x | With `derive` feature |
| schemars | 1.0 | Required by rmcp `#[tool]` macro for parameter schema generation |
| clap | 4.x | CLI argument parsing |
| tracing | 0.1 | With `tracing-subscriber` 0.3 for output |
| nix | 0.30 | For SIGTERM signals to child processes (features: process, signal) |

## Axum Patterns

### State Management
Use `State<Arc<AppState>>` — push Mutex/RwLock inside to individual fields, never wrap the whole state:
```rust
#[derive(Clone)]
struct AppState {
    db: Arc<libsql::Database>,
    coordinator: Arc<RwLock<Coordinator>>,
    // cheap to clone handles
}
```

### Middleware Stack
Tower ServiceBuilder — TraceLayer outermost, then Timeout, then Compression, then CORS. Custom auth via `axum::middleware::from_fn`.

### SSE
Built into axum default features via `axum::response::sse::Sse`. But for MCP transport, rmcp handles SSE internally — raw SSE only needed for custom streaming endpoints.

## libSQL / Turso

### Embedded Replica API
```rust
let db = Builder::new_remote_replica(
    "djinn.db",                      // local file
    "libsql://your-db.turso.io",     // remote primary
    "auth-token",
)
.sync_interval(Duration::from_secs(60))
.build().await?;
db.sync().await?; // initial catch-up
let conn = db.connect()?; // cheaply clonable
```

### CRITICAL: Cross-Process File Sharing is UNSAFE

**Turso docs explicitly warn: "Do not open the local database while the embedded replica is syncing. This can lead to data corruption."**

The embedded replica uses a custom WAL implementation with Turso-specific frame formats. A second process (desktop) opening the same file during sync will race with the WAL writer.

**Options for desktop reads:**
1. Desktop reads via server HTTP API (MCP tools) — server owns the file exclusively
2. Desktop gets its own independent embedded replica syncing from Turso cloud
3. Run `sqld` as a sidecar — both processes connect over HTTP

**For local-only mode (no Turso cloud):** Use `Builder::new_local()` with standard SQLite WAL. Multiple processes CAN read a standard SQLite WAL file safely. The restriction is specific to Turso's custom replication WAL.

### WAL Mode (Single Process)
Within one process, multiple `db.connect()` connections can read concurrently. Writes serialize at connection level. Standard SQLite WAL behavior.

### DiskANN Vector Search
Available via SQL (`CREATE INDEX ... libsql_vector_idx(...)`, `vector_top_k()`). No special Rust types needed — use raw `conn.execute()` / `conn.query()`. Production-capable in C implementation. Pure Rust rewrite in progress (issue #832).

## MCP SDK (rmcp)

### Tool Registration
```rust
#[tool_router]
impl DjinnServer {
    #[tool(description = "Create a task")]
    async fn task_create(
        &self,
        Parameters(params): Parameters<TaskCreateParams>,
    ) -> Result<CallToolResult, McpError> { ... }
}

#[tool_handler]
impl ServerHandler for DjinnServer { ... }
```

### Streamable HTTP Setup
`StreamableHttpService` nests into axum router at `/mcp`. Uses `LocalSessionManager` for per-client sessions. Factory closure creates one server instance per connection — shared state via Arc.

## Tokio Patterns

### Subprocess Spawning
- `tokio::process::Command` with `kill_on_drop(true)` — **required** to prevent zombies
- No built-in SIGTERM in tokio — use `nix::sys::signal::kill()` for graceful shutdown
- Always separate stdin writer and stdout reader into different tokio tasks (deadlock prevention)

### Graceful Shutdown
`CancellationToken` + `TaskTracker` from `tokio-util`. Do NOT use `tokio-graceful-shutdown` — adds unnecessary complexity. This is the pattern rmcp itself uses.

### Event-Driven Coordinator Loop
`tokio::select!` with cancellation token + channel receivers. Recovery on 30s interval tick as safety net.

## Database Migrations

**Hand-rolled with `include_str!` SQL files.** Neither sqlx nor refinery have native async libsql support.

```rust
const MIGRATIONS: &[(&str, &str)] = &[
    ("001_init", include_str!("../migrations/001_init.sql")),
    ("002_agents", include_str!("../migrations/002_agents.sql")),
];
// Track in _migrations table, apply idempotently at startup
```

~30 lines, zero extra dependencies, works perfectly with libsql's async API.

## Testing

- `#[tokio::test]` for async tests
- `Builder::new_local(":memory:")` for per-test DB isolation
- `tower::ServiceExt::oneshot()` for axum integration tests
- `tokio::test(start_paused = true)` for time-dependent logic
- `tokio_test::io::Builder` for mocking subprocess I/O

## Confidence Flags

- **[LOW]** rmcp 0.17.0 — reported but unverified; use 0.16.0 as safe floor
- **[LOW]** `Builder::new_synced_database` offline-writes — public beta, "no durability guarantees"
- **[MEDIUM]** DiskANN pure-Rust rewrite — may have landed by March 2026

## Relations
- [[brief]] — project context
- [[Language Selection — Compiler as AI Code Reviewer]] — language decision
- [[Embedded Database Survey]] — database selection rationale
- [[Rust Agentic Ecosystem Survey]] — broader ecosystem context
- [[Features Research]] — feature needs inform stack choice
- [[Architecture Research]] — architecture patterns drive stack decisions
- [[Pitfalls Research]] — risks to consider