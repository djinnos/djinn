---
tags:
    - research
    - synthesis
title: Research Summary
type: research
---
# Research Summary — Cross-Cutting Synthesis

## Convergent Themes

### 1. The Desktop is a Thin Shell — Architecture Confirms This
All four dimensions converge on the same conclusion: the new desktop is architecturally simpler than the old one. The server handles all domain logic, execution, and state. The desktop is a webview that renders SSE-driven state and sends MCP calls. This means:
- No Rust-side domain logic needed (only process lifecycle + auth)
- Frontend complexity is the primary challenge (UI patterns, state management)
- Tauri's limited IPC is not a constraint — we barely use it

### 2. SSE is the Backbone — Get It Right
Stack, features, and architecture research all point to SSE as the critical integration layer:
- Server pushes full-entity events (no follow-up reads needed — per server ADR-002)
- Zustand stores mirror server state directly from event payloads
- TanStack Query handles request-driven data (settings, providers)
- Must handle: CORS headers (`Access-Control-Allow-Origin: *`), reconnection, Last-Event-ID replay
- React Query + SSE invalidation is the recommended pattern for multi-feature apps

### 3. Sidecar Lifecycle is the Hardest Infrastructure Problem
Pitfalls and architecture research converge: managing the server binary is ~200 lines of production boilerplate with no official plugin. This is the highest-effort infrastructure task:
- Port conflict detection, health polling, crash restart with backoff
- Cross-platform signal handling (Unix vs Windows)
- Orphan process cleanup
- macOS quarantine removal for bundled binaries

### 4. Cross-Platform Webview Differences Require Early Testing
Stack and pitfalls research highlight WebKitGTK (Linux) as the weakest link:
- No WebRTC/WebGPU, CSS animation issues, old WebKit versions
- Windows WebView2 has IPC performance issues (~200ms for large payloads)
- macOS needs JIT entitlements or WKWebView crashes silently
- **Test on all three platforms from the start, not at the end**

## Tensions and Resolutions

### Tension: shadcn/ui simplicity vs drag-and-drop complexity
shadcn provides beautiful static UI components. Kanban drag-and-drop requires @dnd-kit which has its own state management. **Resolution:** Use @dnd-kit for the kanban only. Keep all other UI in shadcn/Base UI. Don't over-abstract the DnD layer.

### Tension: Zustand (SSE state) vs TanStack Query (request state)
Two state management systems could create confusion. **Resolution:** Clear boundary — Zustand owns real-time state (tasks, epics, server status) driven by SSE. TanStack Query owns request-response state (settings, providers, one-time fetches). SSE events can invalidate TanStack Query cache for cross-cutting updates.

### Tension: Auto-update simplicity vs startup crash risk
The updater runs inside the app. A startup crash strands users. **Resolution for v1:** Keep `setup()` minimal and panic-free. Move all startup logic to async tasks. Accept the architectural risk for v1. **Resolution for v2:** Separate launcher binary that can recover from startup crashes.

### Tension: macOS notarization cost vs Linux testing needs
macOS CI runners cost 10x Linux. **Resolution:** Run macOS builds only on release branches. Run Linux E2E tests on every PR (cheap). Manual macOS testing during development. Automated macOS signing/notarization only for releases.

## Open Questions for Requirements

1. **Port allocation strategy**: Fixed port or dynamic? Dynamic is safer (no conflicts) but requires the frontend to discover the port.
2. **Auth flow**: Is Clerk JWT required for v1, or can the desktop run without auth initially? The server supports auth-disabled mode.
3. **Offline behavior**: What happens when the server is down? Show error state only, or cache last-known state?
4. **Theme**: Dark mode only, light mode only, or system-sync from day one?
5. **Linux minimum**: Ubuntu 22.04 only, or attempt 20.04 support? (WebKitGTK 4.1 availability)

## Recommendations for Roadmap

1. **Start with project scaffolding + sidecar lifecycle** — this is the hardest infrastructure and unblocks everything else
2. **Build SSE integration early** — it's the backbone for all UI features
3. **Kanban first, roadmap second** — kanban is the primary interaction; roadmap is read-only visualization
4. **Settings and onboarding can be built in parallel** — no dependency on SSE/kanban
5. **Auto-update and CI pipeline should be set up before first beta** — not last
6. **Cross-platform testing from milestone 1** — don't discover WebKitGTK issues late

## Relations
- [[Project Brief]] — project vision and constraints
- [[Stack Research]] — technology stack analysis
- [[Features Research]] — feature patterns and library choices
- [[Architecture Research]] — system architecture patterns
- [[Pitfalls Research]] — risks and mitigations
- [[V1 Requirements]] — requirements derived from this synthesis
- [[Roadmap]] — delivery plan informed by these recommendations