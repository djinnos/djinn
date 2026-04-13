---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap
type: design
tags: ["docker","web-ui","openviking","roadmap","epic-7izs"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap

## Status
Epic `7izs` remains open. The goal is **not** yet complete: core foundation work is still in progress and the OpenViking migration has only reached the seam/bootstrap stage.

## Architectural source
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
- [[reference/adr-043-roadmap-active-decomposition-status]]
- [[cases/recreated-missing-canonical-roadmap-note-for-docker-openviking-epic-memory]]
- [[cases/singleton-note-writes-from-planner-worktree-target-duplicates-instead-of-canonical-project-root-files]]

## Completed work
- `h3p6` closed: browser-compatible frontend runtime boundary landed, giving the web UI a shared non-Electron transport/runtime seam.
- `sgs2` closed: restored the missing canonical roadmap memory reference and repaired epic/task metadata drift.

## Active foundation work
- `rpgb` — serve the React app from `djinn-server`
- `2744` — replace native pickers with server-backed filesystem browsing
- `4a4t` — create the OpenViking memory backend seam and bootstrap client

These are the critical enablement tasks. The epic cannot close until the browser-hosted shell and OpenViking migration path both exist end-to-end.

## Current wave plan
This wave focuses on sequencing the next implementation batch behind the active foundation work rather than opening more parallel work than the codebase can safely absorb.

### Web deployment track
1. `rpgb` must land before Docker packaging is considered complete, because the server image/compose stack must package and serve the built SPA.
2. `aijd` depends on `rpgb`.
3. `2744` continues independently once the browser runtime boundary is available.
4. `24v4` follows as the remaining Electron-to-server migration for SSH/deploy flows.

### OpenViking migration track
1. `4a4t` establishes the backend seam and client bootstrap.
2. `vce4` depends on `4a4t` for shadow reads and parity logging.
3. `d4qf` depends on `4a4t` and `vce4` for write cutover and bootstrap import.
4. `ow2x` depends on `d4qf` because URI emission should switch only after OpenViking-backed read/write paths exist.
5. `ow2c` depends on `d4qf` and `ow2x` because cleanup belongs after authoritative cutover and mixed-format compatibility are in place.

## Wave task set
Existing open tasks already define the next wave and remain the correct implementation slices; this planning pass primarily tightened sequencing and memory-linking rather than adding more parallel work:
- `aijd` — Docker Compose packaging after SPA serving
- `24v4` — browser/server-owned SSH and deployment APIs
- `vce4` — OpenViking dual-read shadow
- `d4qf` — OpenViking write/bootstrap migration
- `ow2x` — `viking://` memory_refs transition
- `ow2c` — legacy memory cleanup after cutover

## 2026-04-13 planning wave refresh
- Reassessed epic `7izs`: still open because SPA serving (`rpgb`), filesystem picker migration (`2744`), and OpenViking seam/bootstrap (`4a4t`) are still in progress, and the downstream Docker/OpenViking cutover tasks have not landed.
- Confirmed the next implementation wave is already represented on the board by existing worker tasks rather than needing additional decomposition.
- Tightened sequencing on the board: `aijd` blocked by `rpgb`; `vce4` blocked by `4a4t`; `d4qf` blocked by `4a4t` + `vce4`; `ow2x` blocked by `d4qf`; `ow2c` blocked by `d4qf` + `ow2x`.
- Normalized active task memory refs so the in-flight foundation tasks and cleanup task point at this roadmap note.

## Planner notes
- Prefer this design note over the singleton `[[roadmap]]` for epic `7izs` planning, because the singleton roadmap is currently used by other epics and has known drift risk.
- Do not schedule cleanup (`ow2c`) before the OpenViking path is authoritative.
- Do not treat Docker packaging as complete until the runtime serves the frontend bundle from the server image.

## Exit criteria for epic closure
Epic `7izs` can close only when:
- the web UI is served by `djinn-server` in Docker,
- file-picker and SSH/deploy browser flows no longer require Electron,
- OpenViking is authoritative for memory read/write flows,
- task/epic `memory_refs` can operate on `viking://` URIs with compatibility during migration,
- legacy memory-only systems targeted by ADR-053 are retired.
