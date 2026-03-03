---
tags:
    - adr
    - lifecycle
    - daemon
    - updates
title: Server Lifecycle — Desktop-Managed Daemon with Graceful Restart
type: adr
---
# ADR-005: Server Lifecycle — Desktop-Managed Daemon with Graceful Restart

## Status: Accepted

## Context

The server is a Rust binary bundled inside the Electron desktop app. The desktop spawns the server as a child process. When the desktop auto-updates (Electron's built-in updater), it may include a new server binary. The running server must be gracefully replaced without losing state or corrupting in-progress work.

## Decision

The desktop manages the server lifecycle as a daemon. On update, the desktop performs a graceful restart: signal the old server to shut down, wait for it to finish, then start the new binary.

### Startup

```
Desktop launches:
  1. Extract server binary from app resources
  2. Pass Clerk JWT and config (port, DB path) as CLI args or env vars
  3. Spawn server as child process (stdio piped for health monitoring)
  4. Wait for health check (server responds to system_ping)
  5. Connect MCP session
```

### Graceful shutdown

```
Desktop requests shutdown:
  1. Send SIGTERM to server process (or platform equivalent)
  
Server receives SIGTERM:
  2. Stop accepting new MCP connections
  3. Stop dispatching new agent tasks
  4. For each running agent session:
     a. Send SIGTERM to agent subprocess
     b. Wait up to 5s for agent to WIP-commit and exit
     c. If agent doesn't exit, SIGKILL
  5. Release all worktrees (don't remove — preserve for resume)
  6. Run final WAL checkpoint
  7. Close DB connection
  8. Exit cleanly (exit code 0)
```

### Graceful restart (for updates)

```
Desktop detects new server binary:
  1. Signal old server: SIGTERM
  2. Wait for old server to exit (timeout: 30s)
  3. If timeout: SIGKILL old server
  4. Start new server binary with same config
  5. New server reads state from DB → resumes
  6. New server detects interrupted agents (in_progress tasks with no running session)
  7. Board reconciliation heals stale tasks → re-dispatches
```

### State survival

**All state is in the DB.** Nothing in memory needs to survive a restart:
- Task states, agent session records, model health → all in `~/.djinn/djinn.db`
- Worktrees → preserved on disk, not removed during shutdown
- Agent work → WIP-committed to git branches before shutdown

The new server process reads the DB and picks up exactly where the old one left off. Board reconciliation (TASK-09) detects any inconsistencies (e.g., tasks marked in_progress but no running session) and heals them.

### Desktop responsibility

The desktop is the supervisor:
- Monitors server process (exit codes, health checks)
- Restarts server if it crashes unexpectedly
- Handles update sequencing (shutdown old → start new)
- Passes fresh Clerk tokens on restart

### Platform considerations

| Platform | Shutdown signal | Notes |
|---|---|---|
| Linux / macOS | SIGTERM | Standard Unix signal handling |
| Windows (WSL) | SIGTERM via WSL | Server runs in WSL, desktop on Windows |
| Windows (native, v2+) | Named pipe or TerminateProcess | v2 if native Windows support is added |

## Consequences

### Positive
- Zero-downtime updates: graceful shutdown preserves all work, new server resumes from DB
- No state migration between versions — DB migrations handle schema changes
- Board reconciliation auto-heals any inconsistencies from interrupted shutdown
- Simple mental model: desktop is the supervisor, server is the worker

### Negative
- Graceful shutdown has a timeout (30s) — if agents don't exit in time, they get SIGKILL and may lose uncommitted work
- Desktop must handle the update sequencing correctly (race conditions if two desktops try to manage the same server)
- WIP commits may leave "messy" branches that agents need to clean up on re-dispatch

### Open questions
- Should the server support a "pause" mode (stop new dispatches but keep existing sessions running) for faster restarts when no agents are active?
- Should the desktop pre-extract the new binary before signaling shutdown to minimize downtime?

## Relations
- [[Project Brief]] — server lifecycle not previously specified
- [[V1 Requirements]] — adds LIFE-* requirements
- [[Authentication — Clerk JWT Validation]] — ADR-004, tokens passed on spawn
- [[Database Layer — rusqlite over libsql/Turso]] — ADR-002, state survival via DB