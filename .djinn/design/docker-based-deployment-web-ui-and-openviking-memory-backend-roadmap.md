---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap
type: design
tags: ["docker","web-ui","openviking","roadmap","epic-7izs"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap

## Status
In progress. Epic `7izs` is not complete: foundational browser-runtime work has landed (`h3p6`), roadmap-reference repair landed (`sgs2`), and the next execution wave is already represented by active worker tasks. No epic closure is warranted.

## Architecture anchor
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]] defines the target shape: Docker Compose deployment, browser-delivered UI, and phased OpenViking migration.
- `memory_refs` must keep working during migration; use compatibility layers rather than big-bang replacement.

## Completed work
1. **Browser runtime boundary landed** — `h3p6`
   - Frontend can now run without mandatory `window.electronAPI`.
   - Shared transport/runtime seams exist for browser mode.
2. **Roadmap reference repair landed** — `sgs2`
   - Epic/task references were normalized after the missing-note incident.

## Active wave
### Web / deployment track
- `rpgb` — Serve the React web app from `djinn-server`
- `2744` — Replace native project/file pickers with server-backed filesystem browsing
- `24v4` — Move SSH connection and deployment flows behind server-owned browser APIs
- `aijd` — Package `djinn-server` and OpenViking with Docker Compose

### Memory migration track
- `4a4t` — Create the OpenViking memory backend seam and bootstrap client
- `vce4` — Add OpenViking dual-read shadow for memory reads and context retrieval
- `d4qf` — Migrate memory writes and bootstrap data into OpenViking
- `ow2x` — Transition task and epic `memory_refs` to `viking://` URIs with legacy compatibility
- `ow2c` — Retire legacy memory confidence, watcher, and obsolete MCP tool surfaces after OpenViking cutover

## Required sequencing
### Web / deployment ordering
1. `rpgb` should land before `aijd` so container packaging targets the real static-asset serving path.
2. `24v4` depends on the browser runtime boundary already landed in `h3p6`, but can proceed independently of `2744` as long as it stays focused on SSH/deploy APIs.
3. `2744` can proceed in parallel with `rpgb`, but must target the shared browser runtime seam rather than reintroducing Electron-specific code.

### Memory migration ordering
1. `4a4t` is the prerequisite seam for all OpenViking follow-ons.
2. `vce4` depends on `4a4t`.
3. `d4qf` depends on `vce4` so write cutover follows shadow-read parity work.
4. `ow2x` depends on `d4qf` because `viking://` references only make sense once OpenViking-backed content is authoritative enough for mixed-format resolution.
5. `ow2c` depends on `ow2x` and final cutover completion.

## Planner assessment for this wave
The epic already has a full active wave on the board, so this planning pass should **tighten roadmap/sequencing rather than add duplicate tasks**. The priority is to keep the existing tasks ordered correctly and avoid board churn while `rpgb`, `2744`, and `4a4t` are still in flight.

## Exit criteria for epic closure
Do not close epic `7izs` until all of the following are true:
- `djinn-server` serves the SPA in production and Docker packaging is documented and working.
- Browser UI no longer depends on Electron-only pickers or SSH/deploy IPC.
- OpenViking is authoritative for memory reads/writes.
- `memory_refs` support `viking://` with legacy compatibility during migration.
- Legacy memory confidence/watcher/obsolete MCP surfaces are removed or disabled.

## Relations
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
- [[roadmap]]
- [[reference/adr-043-roadmap-active-decomposition-status]]
