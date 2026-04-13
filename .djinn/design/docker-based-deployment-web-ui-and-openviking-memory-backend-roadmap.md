---
title: Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap
type: design
tags: ["adr-053","docker","web-ui","openviking","roadmap"]
---

# Docker-Based Deployment, Web UI, and OpenViking Memory Backend — Roadmap

## Status
Epic `7izs` remains open. The architectural direction from [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]] is still valid, but the implementation is only partially landed.

## Completed work
- Browser-compatible frontend runtime boundary landed via task `h3p6`, giving the web app a non-Electron transport/bootstrap seam.
- Canonical roadmap/memory-ref repair landed via task `sgs2`, restoring `design/docker-based-deployment-web-ui-and-openviking-memory-backend-roadmap` as the intended epic roadmap permalink.

## In-flight work
- `2744` — replace native project/file pickers with server-backed filesystem browsing. Worker implementation is in progress and currently blocked in verification by unrelated `djinn-agent` snapshot drift rather than picker-specific failures.
- `4a4t` — create the OpenViking memory backend seam and bootstrap client.
- `wpe0` — repair unrelated `djinn-db` baseline test failures so epic tasks can verify against a green server baseline.

## Next-wave sequencing

### Wave A — Make the browser-hosted deployment viable
1. `wpe0` must land first to restore a trustworthy verification baseline for server-side work.
2. `rpgb` can then land the Rust static asset / SPA hosting path in `djinn-server`.
3. `2744` and `24v4` complete the remaining browser-only UX seams by replacing native pickers and Electron-owned SSH/deploy flows.
4. `aijd` packages the server + frontend + OpenViking stack in Docker Compose once server hosting and core runtime seams are present.

### Wave B — OpenViking migration
1. `4a4t` establishes the memory backend seam and client bootstrap.
2. `vce4` adds OpenViking dual-read shadowing for read/context paths.
3. `d4qf` migrates writes and bootstrap/import behavior.
4. `ow2x` switches task/epic `memory_refs` toward `viking://` URIs with compatibility.
5. `ow2c` removes legacy confidence, watcher, and obsolete MCP memory surfaces after cutover.

## Active task map
- Browser/runtime foundation: `h3p6` ✅
- Static frontend hosting: `rpgb`
- Browser filesystem picker: `2744`
- Browser SSH/deploy flows: `24v4`
- Docker packaging: `aijd`
- OpenViking backend seam: `4a4t`
- Dual-read shadow: `vce4`
- Write/bootstrap migration: `d4qf`
- `memory_refs` URI transition: `ow2x`
- Legacy memory cleanup: `ow2c`
- Verification-baseline repair: `wpe0`

## Planning notes
- Do not create additional broad ADR-053 worker tasks until the current queue shrinks; the epic already has enough decomposed work for the next execution wave.
- Prefer blocker relationships over new decomposition rows where work is already represented.
- If `djinn-agent` prompt snapshot churn continues to block unrelated verification after `wpe0`, split that into a focused baseline-remediation task rather than broadening feature tasks.

## Relations
- [[decisions/adr-053-docker-based-deployment-web-ui-and-openviking-memory-backend]]
- [[reference/adr-043-roadmap-active-decomposition-status]]
- [[roadmap]]
