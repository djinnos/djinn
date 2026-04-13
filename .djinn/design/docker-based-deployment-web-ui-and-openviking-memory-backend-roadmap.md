---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap
type: design
tags: ["epic-roadmap","adr-053","docker","web-ui","openviking"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap

## Status
Epic [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]] is **not complete**. No implementation tasks have landed yet; the board only contains this planning task. The codebase still ships an Electron shell, relies on Electron IPC for key UI flows, serves only API/MCP routes from `server/src/server/mod.rs`, and still uses the legacy `NoteRepository`/memory MCP surface in `server/` and `server/crates/djinn-mcp/`.

## Current repo facts
- Desktop packaging is still Electron-based (`desktop/package.json`, `desktop/electron/main.ts`, `desktop/electron/ipc-handlers.ts`).
- Frontend code still imports Electron wrappers broadly (`desktop/src/electron/commands.ts`, `desktop/src/electron/shims/*`).
- The Rust server currently exposes health/events/MCP/project-management routes, but no SPA static-file serving or browser-oriented replacement endpoints for native pickers/window APIs (`server/src/server/mod.rs`, `server/src/server/project_tools.rs`).
- Memory operations still center on `djinn_db::NoteRepository`, `memory_build_context`, `memory_health`, `memory_broken_links`, `memory_orphans`, task confidence, and the KB watcher (`server/crates/djinn-mcp/src/tools/memory_tools/*`, `server/src/task_confidence.rs`, `server/src/watchers/kb.rs`).
- No Dockerfile or compose stack exists in the repo root or `server/`.

## Decomposition strategy
Sequence this epic as three waves so workers can land vertical slices without trampling each other.

### Wave 1 — Foundation and seams
1. Add server static-asset hosting so the Rust server can serve the built web app alongside existing HTTP/MCP APIs.
2. Extract the frontend runtime boundary away from Electron-only shims so the React app can run in a normal browser against the Rust server.
3. Add server-side filesystem browsing/import APIs to replace the native directory/file pickers used by project onboarding and connection settings.
4. Introduce a memory backend seam plus an OpenViking client/configuration slice without changing user-visible memory behavior yet.
5. Add Docker packaging and a compose stack wiring Djinn + OpenViking, using the new server/web build outputs.

### Wave 2 — Web-only UX completion
- Remove remaining Electron-only flows (window chrome, auth/token/local integration, SSH/remote helpers that must move server-side or be dropped).
- Switch the shipped app/docs to browser-first usage.
- Reduce or eliminate Electron build/package scripts once the browser path is production-ready.

### Wave 3 — Memory migration
- Implement dual-read shadowing against OpenViking.
- Migrate writes and `memory_refs` URI handling to `viking://`.
- Remove confidence-scoring and legacy knowledge-base maintenance features deprecated by ADR-053.
- Delete legacy `NoteRepository`-specific codepaths, watchers, and surplus MCP tools.

## Wave 1 tasks
This wave creates five concrete worker tasks:
1. Server SPA/static hosting scaffold.
2. Frontend browser-runtime adapter replacing Electron-only transport assumptions.
3. Filesystem browsing/import HTTP APIs plus web picker integration.
4. Memory backend seam + OpenViking client bootstrap.
5. Dockerfile/compose packaging for Djinn + OpenViking.

## Exit criteria for this wave
- The server can serve a built frontend bundle.
- The frontend has a browser-compatible runtime path for basic server communication.
- At least one onboarding/project-selection flow works without native file dialogs.
- OpenViking integration is represented by a real backend seam and client bootstrap instead of being only an ADR.
- A local operator can start Djinn + OpenViking with Docker Compose.

## Follow-up notes
- Keep tasks narrow and file-seamed to avoid simultaneous edits to the same Electron/server bootstrap files.
- Do **not** attempt full memory switchover in the same wave as seam extraction; land the integration seam first, then migrate behavior in later waves.
- Preserve MCP compatibility during the migration period; browser delivery and backend swap should not break the planner/worker toolchain mid-epic.

## Relations
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
- [[brief]]
