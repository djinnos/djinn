---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap
type: design
tags: ["docker","web-ui","openviking","roadmap","epic-7izs"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap

## Status
Epic `7izs` remains **open**. The architectural direction in [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]] is accepted, but the codebase still contains substantial Electron-specific UI/runtime behavior and the legacy `NoteRepository`-based memory system.

## Current Board State

### Active / recently active wave
- `rpgb` — serve the React web app from `djinn-server`
- `h3p6` — introduce a browser-compatible frontend runtime boundary
- `2744` — replace native project/file pickers with server-backed filesystem browsing
- `4a4t` — create the OpenViking memory backend seam and bootstrap client
- `aijd` — package `djinn-server` and OpenViking with Docker Compose

### What this wave establishes
1. Browser delivery of the SPA from the Rust server.
2. A shared frontend runtime seam so the app can run without `window.electronAPI` for core transport/bootstrap flows.
3. The first browser-native filesystem picker flow.
4. An initial OpenViking integration seam so later phases can migrate incrementally instead of as a big-bang swap.
5. Container packaging once the server/static-hosting and memory-backend seams exist.

## Why the epic is not complete
Code search confirms the repo still contains:
- Electron main/preload/IPC shell code under `desktop/electron/*`
- Electron-only commands still used by frontend flows such as `selectDirectory`, `selectFile`, and SSH connection management
- Legacy memory systems including `NoteRepository`, `task_confidence`, `watchers/kb.rs`, and MCP schemas/tests for tools ADR-053 plans to retire
- No completed transition of `memory_refs` to `viking://` URIs yet

So the epic is in active migration, not closure.

## Sequencing
- `2744` should stay sequenced behind `h3p6` because the browser picker depends on the shared non-Electron runtime boundary.
- `aijd` should stay sequenced behind `rpgb` and `4a4t` because the Compose stack must package the browser-served server and the OpenViking sidecar contract it depends on.

## Next Wave After Current Foundations

### Wave 2A — Browser deployment parity
1. **Move SSH/deployment flows to server-owned APIs**
   - Replace Electron-managed SSH host/tunnel/deploy orchestration with server-side HTTP APIs and browser-compatible UI flows.
   - This covers the remaining major web-migration gap after the runtime boundary and picker work.

### Wave 2B — OpenViking phased migration
2. **Dual-read shadow for read/search/list/build-context flows**
   - Route read-oriented memory operations through the new backend seam with parity checks while preserving the legacy implementation.
3. **Write-path migration and data switchover tooling**
   - Add migration/bootstrap tooling, dual-write or cutover semantics, and operational docs for moving persisted memory into OpenViking.
4. **`memory_refs` URI transition**
   - Introduce `viking://` URI handling for tasks/epics while keeping legacy permalink resolution during migration.
5. **Legacy memory cleanup**
   - Remove obsolete confidence scoring, watcher/housekeeping, and MCP tools that ADR-053 explicitly replaces once the cutover is complete.

## Exit Criteria for Epic Closure
The epic can close only when all three are true:
1. Djinn is runnable as a browser-served Docker/Compose deployment without Electron-required user flows.
2. OpenViking is the effective memory backend for supported memory operations, with migration/compatibility handled.
3. Legacy Electron-only packaging and legacy memory subsystems called out by ADR-053 are removed or intentionally retired.

## Relations
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
- [[roadmap]]
- [[reference/adr-043-roadmap-active-decomposition-status]]
