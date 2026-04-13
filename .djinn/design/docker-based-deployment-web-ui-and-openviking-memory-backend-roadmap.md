---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap
type: design
tags: ["roadmap","design","docker","web-ui","openviking","epic-7izs"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend Roadmap

Related ADR: [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
Epic: `7izs` — Docker-Based Deployment, Web UI, and OpenViking Memory Backend
Canonical permalink: `design/docker-based-deployment-web-ui-and-openviking-memory-backend-roadmap`

## Objective

Deliver the ADR-053 migration from the current Electron desktop + custom memory stack to:
- a Docker Compose deployment model,
- a browser-served web UI hosted by `djinn-server`, and
- an OpenViking-backed memory architecture introduced in phases.

This note is the canonical roadmap artifact referenced by epic `7izs` and its child tasks.

## Current active wave

The active wave is establishing the foundational seams needed for the migration:

1. **Serve the React app from `djinn-server`**
   - Active task: `rpgb`
   - Goal: add static asset hosting and SPA fallback without breaking existing API/MCP routes.

2. **Introduce a browser-compatible frontend runtime boundary**
   - Active task: `h3p6`
   - Goal: remove hard dependencies on `window.electronAPI` from foundational frontend runtime flows.

3. **Create the OpenViking memory backend seam and bootstrap client**
   - Active task: `4a4t`
   - Goal: add a backend abstraction and initial OpenViking bootstrap/config wiring while preserving the legacy implementation.

4. **Replace native project/file pickers with server-backed filesystem browsing**
   - Active task: `2744`
   - Goal: provide safe server-side listing endpoints and migrate at least one picker flow to a browser-native UX.

5. **Package djinn-server and OpenViking with Docker Compose**
   - Open task: `aijd`
   - Goal: add a Dockerfile and compose stack that run Djinn with OpenViking, persistent volumes, and documented setup steps.

## Wave sequencing

### Wave 1 — foundational seams
- Static frontend serving in `djinn-server`
- Browser runtime boundary for the frontend
- Initial OpenViking backend seam/bootstrap
- First server-backed picker flow
- Docker Compose packaging

### Wave 2 — Electron replacement completion
- Remove remaining Electron-only integrations
- Move any residual shell/server-side behaviors behind HTTP/server APIs
- Tighten browser-first startup/configuration flows

### Wave 3 — OpenViking migration phases
- dual-read/shadow integration
- migration tooling and write switchover
- `memory_refs` URI transition support
- retirement of confidence-scoring and legacy memory subsystems
- final cleanup of superseded tools/watchers/storage paths

## Planner notes

- Use this permalink for epic/task `memory_refs`: `design/docker-based-deployment-web-ui-and-openviking-memory-backend-roadmap`
- If future waves need to replace this note, update epic `7izs` and all open/in-progress child tasks in the same planning pass.
- Current child-task references are intended to keep pointing here; the repair for the missing note is to restore this canonical permalink rather than mint a new one.
