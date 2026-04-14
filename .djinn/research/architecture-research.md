---
title: Architecture Research
type: research
tags: []
---

# Architecture Patterns for Djinn Server (March 2026)

## 1. Coordinator: Actor Hierarchy over God Object

The Go server's coordinator has 50+ fields. Prevent this with hand-rolled Tokio actors (Alice Ryhl pattern):

### Actor + Handle Split
Every actor has a **private struct** (owns state, runs in a spawned task) and a **public handle** (cloneable, holds only an mpsc::Sender):

```rust
struct TaskCoordinator {
    receiver: mpsc::Receiver<CoordinatorMsg>,
    db: Arc<Database>,
    agents: HashMap<AgentId, AgentHandle>,
}

#[derive(Clone)]
pub struct CoordinatorHandle {
    sender: mpsc::Sender<CoordinatorMsg>,
}
```

### Decomposed Actor Hierarchy
```
SystemSupervisor
├── CoordinatorActor  — dispatch decisions, state transitions only
├── AgentSupervisor   — spawns/monitors agent subprocesses
│   ├── AgentActor[1] — wraps one agent subprocess
│   └── AgentActor[N]
├── GitActor          — serialized git operations
└── EventBroadcaster  — fans out events to MCP sessions
```

**Rules:**
- Each actor's `enum Msg` has ≤ 15 variants
- If an actor knows about >3 other actor types, split it
- Actors communicate via handles, never via `Arc<Mutex<SharedState>>`
- Use `ractor` crate for actors that need restart-on-panic supervision (agent actors)
- Use hand-rolled Tokio for simple actors (git, events, coordinator)

### Bounded Channel Deadlock Rule
Never create a cycle of bounded channels. If actor A sends to B and B sends back to A, at least one direction must use `try_send` or unbounded.

## 2. Turso / Desktop Sync Architecture

### Critical Finding: Embedded Replicas Require Turso Cloud

Turso embedded replicas (`Builder::new_remote_replica`) sync via Turso Cloud. There is no purely local embedded replica mode for cross-process reads.

### Deployment Architectures by Mode

**Local / WSL (no cloud):**
Run `sqld` as a local sidecar:
```
[Rust Server] ──HTTP──→ [sqld :8080] ←──HTTP── [Electron Desktop]
                              │
                         djinn.db on disk
```
Both processes connect via HTTP. No Turso Cloud account needed. No file-sharing issues.

**VPS (with cloud):**
```
[VPS: Rust Server] ──→ Turso Cloud (primary) ←── [Desktop: embedded replica]
```
Desktop gets microsecond-latency local reads. Writes forwarded through cloud primary.

**Hybrid (local server, future cloud option):**
Start with sqld sidecar. If user enables Turso Cloud sync, server switches to remote replica mode. Desktop can then also be a replica.

### sqld Sidecar Details
```bash
sqld --db-path ~/.djinn/djinn.db --http-listen-addr 127.0.0.1:8080
```
Both Rust server and Electron connect as HTTP clients. Single-writer semantics enforced by sqld.

## 3. State Machine Pattern

### Enum for DB, Typestate for API Correctness

**Enum-based** (persistence layer):
```rust
pub enum TaskStatus {
    Draft, Open, InProgress, NeedsTaskReview, InTaskReview, Approved, Closed,
}

impl TaskStatus {
    pub fn can_transition_to(&self, next: &TaskStatus) -> bool {
        matches!((self, next), ...)
    }
}
```

**Typestate** (service layer — compile-time correctness):
```rust
impl Task<Open> {
    pub fn dispatch(self, agent: AgentId) -> Task<InProgress> { ... }
    // Can't call submit_for_review() — method doesn't exist on Open
}
```

Consider the `statum` crate for zero-boilerplate typestate derive macros.

## 4. Git Operations: Hybrid git2 + CLI

**Use `git2`** (libgit2 bindings) for reads: repo discovery, branch listing, status, diff, ref queries.

**Shell out to `git` CLI** for writes: worktree add/remove, merge, rebase, push/pull.

**Do NOT use gitoxide/gix** — worktree create/remove and merge are explicitly marked INCOMPLETE in their crate-status.md.

### GitActor Pattern
Serialize all git operations through a single actor to avoid concurrent ops:
```rust
enum GitMsg {
    CreateWorktree { branch, path, respond_to },
    RemoveWorktree { path, respond_to },
    MergeBranch { target, source, respond_to },
    GetStatus { path, respond_to },
}
```

## 5. Single DB Schema Pattern

All entities in one `~/.djinn/djinn.db`. Use table prefixes for domain boundaries. Enable FKs explicitly.

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;
```

**ID strategy:** ULIDs (not UUIDs, not integers) — lexicographically sortable by creation time, work well in text columns.

**Activity log:** Append-only `activity_log` table with `event_type`, `payload` (JSON), optional `task_id`. Log survives task deletion (no hard FK constraint).

**Settings:** Key-value table with JSON values. Replaces per-project JSON files.

## 6. MCP Server Architecture

### Per-Session Instances
`StreamableHttpService` factory creates one `DjinnServer` per connected client. Shared resources (DB, coordinator handle) passed via Arc. Per-session state lives in the instance.

### Event Broadcasting
```rust
let (event_tx, _) = broadcast::channel::<ServerEvent>(256);
// Each session subscribes: event_tx.subscribe()
```

### Tool Organization
Group by domain in separate files:
```
mcp/tools/
    task_tools.rs    — create, update, list, transition, blockers
    memory_tools.rs  — write, read, edit, search, catalog
    execution_tools.rs — start, pause, resume, status, kill
    system_tools.rs  — ping, status, settings, projects
```

## 7. WSL Considerations

### Mirrored Networking (Windows 11 22H2+)
In `%USERPROFILE%\.wslconfig`:
```ini
[wsl2]
networkingMode=mirrored
```
Enables `localhost` to work bidirectionally. Electron on Windows connects to server in WSL via `localhost:PORT`.

### Critical Rules
- Keep ALL data files on Linux filesystem (`/home/...`), NEVER on `/mnt/c/` (10-30x I/O penalty)
- Bind server to `0.0.0.0` (not `127.0.0.1`) to work in both WSL NAT and mirrored modes
- Use HTTP over TCP for IPC — Unix domain sockets in WSL are NOT accessible from Windows

## 8. Subprocess Management

- `kill_on_drop(true)` on all agent child processes — prevents zombies on crash
- `.process_group(0)` on Unix to isolate from terminal signals
- Separate stdin writer and stdout reader into different tokio tasks (deadlock prevention)
- Graceful shutdown: SIGTERM → 5s wait → SIGKILL → `child.wait()` (use `nix` crate for SIGTERM)
- `CancellationToken` + `TaskTracker` from `tokio-util` for coordinated shutdown

## Relations
- [[brief]] — project context
- [[Stack Research]] — crate versions and API patterns
- [[Embedded Database Survey]] — database selection rationale
- [[Features Research]] — feature needs inform architecture
- [[Pitfalls Research]] — risks driving architectural choices