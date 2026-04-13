---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap
type: design
tags: ["docker","web-ui","openviking","roadmap","epic-7izs"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap

## Status
Epic `7izs` remains open. The goal is not yet complete: only the browser-runtime boundary (`h3p6`) and roadmap/memory-ref repair (`sgs2`) are closed, while the core deployment, browser parity, and OpenViking migration slices are still in progress or queued.

## Architectural anchor
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]] is the governing decision.
- `memory_refs` should continue to include this note plus `[[roadmap]]` only as a temporary cross-link; this note is the epic-specific plan.

## Completed work
- `h3p6` — browser-compatible frontend runtime boundary landed, giving web-mode transport/runtime seams to build on.
- `sgs2` — restored canonical roadmap memory references for this epic.
- `rpgb` implementation work is substantially present, but final verification is blocked on the repo-green follow-up task `wpe0`.

## Active wave

### 1. Web UI parity and server hosting
- `rpgb` — serve the built React bundle from `djinn-server` with SPA fallback while preserving API/MCP routes. This is now blocked on `wpe0` so it can verify against a green base instead of absorbing unrelated db-fix scope.
- `2744` — replace Electron file/directory pickers with server-backed filesystem browsing for one concrete onboarding/project flow.
- `24v4` — move remaining SSH/deploy flows behind server-owned browser APIs after picker/browser-path foundations stabilize.

### 2. OpenViking migration
- `4a4t` — create the backend seam and OpenViking bootstrap/client wiring. This is the prerequisite for all later migration phases.
- `vce4` — add dual-read shadowing for read/list/search/build-context after `4a4t`.
- `d4qf` — migrate write/bootstrap flows after seam + read shadowing.
- `ow2x` — switch `memory_refs` to mixed-format `viking://` compatibility after write-path migration exists.
- `ow2c` — remove legacy confidence/watcher/obsolete memory MCP surfaces only after OpenViking is authoritative and URI compatibility is in place.

### 3. Packaging/deployment
- `aijd` — package the server and OpenViking with Docker Compose once the server-hosted SPA path (`rpgb`) and OpenViking bootstrap seam (`4a4t`) are both landed.

## Sequencing
- `wpe0` -> `rpgb`
- `2744` -> `24v4`
- `4a4t` -> `vce4` -> `d4qf` -> `ow2x` -> `ow2c`
- `rpgb` + `4a4t` -> `aijd`

## Current board judgment
No new wave of decomposition is needed right now because this planning session already produced the next 3–5 worker tasks for the epic and the board still has active in-progress work (`2744`, `4a4t`, `wpe0`) plus queued follow-ons (`24v4`, `vce4`, `d4qf`, `ow2x`, `ow2c`, `aijd`). The correct action is to preserve sequencing and keep the epic open until the verification blocker and core seams land.

## Exit conditions for epic closure
Close epic `7izs` only after:
1. djinn-server serves the browser UI and browser-only flows no longer depend on Electron,
2. Docker Compose deployment exists and is documented,
3. OpenViking is the authoritative memory backend with legacy memory-only surfaces retired,
4. task/epic memory refs work with `viking://` URIs during/after cutover.
