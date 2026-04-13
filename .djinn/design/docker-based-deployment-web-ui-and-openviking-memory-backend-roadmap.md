---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap
type: design
tags: ["docker","web-ui","openviking","roadmap","epic-7izs"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap

## Status
In progress. Epic `7izs` is **not complete**.

## Goal
Migrate Djinn from an Electron desktop shell plus custom memory system to:
- Docker Compose deployment
- browser-served web UI from `djinn-server`
- OpenViking-backed memory services and `viking://` memory references

This roadmap is the canonical wave-planning note for epic `7izs` and should be preferred over the singleton [[roadmap]] note for this epic's active work.

## Architectural guardrails
- Follow [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]].
- Keep `djinn-server` as the single runtime entrypoint that serves both API routes and the SPA bundle.
- Replace Electron-only capabilities with server-owned HTTP APIs and browser-compatible UI flows.
- Migrate memory in phases: seam/bootstrap → shadow reads → write/import cutover → `viking://` refs → legacy cleanup.
- Do not remove legacy memory systems until OpenViking is authoritative for read and write paths.

## Completed work
- `h3p6` closed: browser-compatible frontend runtime boundary landed.
- `sgs2` closed: canonical roadmap/memory-ref repair completed so active work points at a real roadmap artifact.

## Active wave
### In progress / verifying
- `rpgb` — serve the React web app from `djinn-server`
- `2744` — replace native project/file pickers with server-backed filesystem browsing
- `4a4t` — create the OpenViking memory backend seam and bootstrap client

### Ready next once active items land
1. `aijd` — package `djinn-server` and OpenViking with Docker Compose
2. `24v4` — move SSH connection and deployment flows behind server-owned browser APIs
3. `vce4` — add OpenViking dual-read shadow for memory reads and context retrieval
4. `d4qf` — migrate memory writes and bootstrap data into OpenViking
5. `ow2x` — transition task and epic `memory_refs` to `viking://` URIs with legacy compatibility
6. `ow2c` — retire legacy memory confidence, watcher, and obsolete MCP tool surfaces after OpenViking cutover

## Sequencing
1. **Web delivery foundation**
   - `rpgb` provides SPA/static hosting from the Rust server.
   - `2744` removes the most visible Electron-only picker dependency.
2. **Memory backend foundation**
   - `4a4t` establishes the backend seam and OpenViking bootstrap/config.
3. **Deployment + remaining Electron removal**
   - `aijd` can package the stack once SPA hosting shape and OpenViking boot wiring are real.
   - `24v4` removes SSH/deploy flows from Electron ownership.
4. **OpenViking migration**
   - `vce4` depends on `4a4t`.
   - `d4qf` depends on `vce4` or at minimum the seam from `4a4t`, but should follow shadow-read validation.
   - `ow2x` follows readable/writable OpenViking-backed note flows.
   - `ow2c` is final cleanup after OpenViking becomes authoritative.

## Completion gate
Epic `7izs` is complete only when all three workstreams are done:
- `djinn-server` serves the browser UI and the Docker Compose stack runs Djinn + OpenViking cleanly.
- Electron-owned picker and SSH/deploy flows are replaced by browser/server flows.
- OpenViking is authoritative for reads and writes, `memory_refs` support `viking://`, and legacy memory subsystems are retired.

## Notes for future planning waves
- Do not treat the singleton [[roadmap]] note as authoritative for this epic; it currently tracks ADR-043 work and can diverge from epic `7izs` board state.
- If `rpgb`, `2744`, and `4a4t` all land successfully, the next planning wave should focus on blocker cleanup and ensuring dependency order across `aijd`, `24v4`, `vce4`, `d4qf`, `ow2x`, and `ow2c` rather than creating duplicate tasks.

## Relations
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
- [[cases/recreated-missing-canonical-roadmap-note-for-docker-openviking-epic-memory]]
- [[reference/adr-043-roadmap-active-decomposition-status]]
