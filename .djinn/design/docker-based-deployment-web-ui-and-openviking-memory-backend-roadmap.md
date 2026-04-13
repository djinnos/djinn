---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap
type: design
tags: ["adr-053","docker","web-ui","openviking","roadmap"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap

## Status
In progress. Epic `7izs` is not complete. Foundational work is underway, but the Docker deployment, browser-only replacement flows, and OpenViking migration are only partially landed.

## Architectural anchor
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
- Canonical active-wave reference remains [[roadmap]] for board-wide decomposition context.

## Completed work
- `h3p6` closed: browser-compatible frontend runtime boundary landed.
- `sgs2` closed: repaired missing canonical roadmap reference and epic/task memory refs.
- `rpgb` and `4a4t` both have substantial in-flight implementations submitted and reopened only on unrelated verification drift (`djinn-provider` clippy / snapshot drift), not because the core approach was invalid.

## Current active wave
### In progress
- `rpgb` — serve the React web app from `djinn-server`
- `2744` — replace native pickers with server-backed filesystem browsing
- `4a4t` — create the OpenViking memory backend seam and bootstrap client

### Ready/open behind current foundations
- `aijd` — package `djinn-server` and OpenViking with Docker Compose
- `24v4` — move SSH connection and deployment flows behind server-owned browser APIs
- `vce4` — add OpenViking dual-read shadow for memory reads and context retrieval
- `d4qf` — migrate memory writes and bootstrap data into OpenViking
- `ow2x` — transition task and epic `memory_refs` to `viking://` URIs with legacy compatibility
- `ow2c` — retire legacy memory confidence, watcher, and obsolete MCP tool surfaces after OpenViking cutover

## Wave assessment
The epic should remain open. None of the three ADR-053 workstreams is done end-to-end yet:
1. **Docker deployment** has a task defined but not yet landed.
2. **Web UI migration** has runtime-boundary groundwork done, but static serving, picker replacement, and SSH/deploy browser APIs are still in progress/open.
3. **OpenViking migration** has a seam/bootstrap task in flight, while shadow reads, write cutover, URI transition, and legacy cleanup remain open.

## Sequencing guidance
Use the already-created tasks as the active decomposition wave; do **not** add more worker tasks until this wave drains.

Recommended dependency/order:
1. `rpgb` and `2744` can proceed in parallel on the web stack.
2. `aijd` should consume the frontend-serving outcome from `rpgb` and the OpenViking bootstrap assumptions from `4a4t`.
3. `vce4` should follow `4a4t`.
4. `d4qf` should follow `vce4`.
5. `ow2x` should follow `d4qf`.
6. `ow2c` should follow `ow2x`.
7. `24v4` can proceed independently of the memory migration, but should align with the browser-only deployment model established by `rpgb`.

## Planner note for this session
This planning pass found that the epic already has a full active wave of worker tasks (more than the 3–5 target) created by the previous decomposition session. The correct action in this session is to restore the missing dedicated design roadmap note and capture the current sequencing/completion assessment rather than creating duplicate tasks.
