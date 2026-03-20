---
tags:
    - sync
    - git
    - architecture
title: djinn/ Namespace Git Sync
type: adr
---
# ADR-007: djinn/ Namespace Git Sync

## Status
Accepted

## Context

Djinn needs to sync state across machines for the same user (e.g. two laptops, WSL + Windows desktop). The initial requirement is task state, but other data — knowledge base notes, per-user settings — will need the same treatment. We want a single sync mechanism that can grow without redesign.

Requirements driving this decision:
- SYNC-01 through SYNC-05 (see [[V1 Requirements]])
- No central server dependency (local-first, git is the transport)
- Conflict resolution must be automatic and deterministic
- One channel failing must not block others

## Decision

Use a **`djinn/` git branch namespace** as the universal sync transport. Each category of data gets its own branch under this namespace (e.g. `djinn/tasks`, `djinn/memory`, `djinn/settings`). A single **SyncManager** owns all channels; each channel is registered at startup with its own export, import, and conflict-resolution functions.

### Channel: `djinn/tasks`

The v1 implementation. Per-user JSONL files (`{user_id}.jsonl`) on the `djinn/tasks` branch. Conflict resolution: last-writer-wins on `updated_at`. On push failure (concurrent push from another machine), fetch + rebase and retry with backoff.

### Future channels (not in v1)

| Channel | Format | Conflict strategy |
|---|---|---| 
| `djinn/memory` | One `.md` file per note (same as `.djinn/` layout) | git-native three-way merge |
| `djinn/settings` | Single `{user}.toml` per user | last-writer-wins on `updated_at` |

### Backoff schedule

Exponential: 30s → 60s → 120s → … → 15min cap. Applied per-channel independently.

### Enable / disable

- **Per-machine opt-out:** local DB flag; stops push/pull without deleting the remote branch
- **Team-wide disable:** delete remote branch + set local flag for all machines on next sync attempt

## Consequences

**Good:**
- Single infrastructure to implement and maintain for all sync needs
- Channel isolation: a failure in `djinn/tasks` doesn't block `djinn/memory` sync
- New channels require only an export/import fn + conflict strategy — no changes to the core loop
- Works offline (git push retries when network is available)
- No central server, no database replication protocol

**Neutral:**
- Each channel needs its own branch; a project with many channels will have several `djinn/*` refs
- JSONL format for tasks means the entire user file is replaced on each export (acceptable at current scale)

**Bad:**
- Not real-time (push-based with timer fallback, not streaming)
- Push conflicts under concurrent edits from two machines require rebase + retry (handled by backoff)

## Alternatives Considered

**Single `djinn/sync` branch with subdirectories** — simpler namespace, but a conflict in one category's files would require rebasing the entire branch. Rejected.

**Central sync server / Turso** — eliminated by ADR-002. Not revisited here.

**Git notes (`git notes`)** — non-standard, poor tooling support, complex rebase behavior. Rejected.

## Relations

- [[V1 Requirements]] — SYNC-01 through SYNC-05
- [[Database Layer — rusqlite over libsql/Turso]] — ADR-002, establishes local-first constraint
- [[Server Lifecycle — Desktop-Managed Daemon with Graceful Restart]] — ADR-005, server owns SyncManager lifecycle
