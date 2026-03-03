---
tags:
    - planning
title: Project Brief
type: brief
---
# Djinn Desktop — Project Brief

## Vision

Rebuild the Djinn desktop application as a lightweight Tauri 2.x app that serves as a thin UI shell for the new Rust server. The old Electron + Go architecture is replaced with Tauri + Rust, eliminating the need for Node.js runtime, bundled Chromium, and terminal emulation. The desktop connects to the server over MCP/HTTP and renders real-time state via SSE.

## Problem

The original Djinn desktop (Electron 40 + Go server) carries significant weight:
- 80-150 MB bundle size with bundled Chromium
- 200-400 MB RAM at idle
- Node.js dependency for features no longer needed (node-pty terminals, OpenCode iframe embedding, deep IPC)
- Go server replaced by Rust server with Goose headless agent harness — terminal views are obsolete

The new Rust server (92% complete) fundamentally changes what the desktop needs to do. Agents run headless via Goose, tasks auto-merge to main when complete, phases are eliminated in favor of simpler epic-scoped reviews. The desktop is now purely a visualization and management layer.

## Target Users

Developers using Djinn for AI-assisted project management and autonomous development orchestration. The desktop is their primary interface for:
- Viewing and managing task state (kanban)
- Tracking epic progress (roadmap)
- Configuring projects, providers, and credentials (settings)
- First-run setup (onboarding)

## V1 Scope

### In Scope
- **Kanban board**: Tasks grouped by epic, organized in status columns (open, in_progress, needs_review, approved, closed)
- **Roadmap view**: Epics with tasks grouped under them, epic completion progress visualization
- **Project settings**: Provider/credential management (server's AES-GCM encrypted vault), project git config, agent configuration
- **Onboarding wizard**: First-run setup flow (project registration, provider configuration)
- **Auto-updater**: Tauri updater plugin with GitHub Releases, CI-generated update manifest
- **Server lifecycle**: Spawn Rust server binary as child process, health check, graceful shutdown
- **Real-time updates**: SSE from server → Zustand stores for live UI state

### Out of Scope (V2)
- Agent monitoring / session streaming (server will expose via SSE)
- Session interaction (chat with running agents)
- Knowledge graph / docs browser
- Phase editor (phases eliminated from new server)
- Code diff viewer per task
- Mobile support

## Technology Stack

| Layer | Technology | Notes |
|-------|-----------|-------|
| Desktop shell | Tauri 2.x (v2.10+) | Rust-native, OS webview, ~10 MB bundle |
| Frontend framework | React 19 | Carried from old desktop |
| Styling | Tailwind CSS 4.x | Carried from old desktop |
| Component library | shadcn/ui (Base UI + Mira style) | Base UI primitives instead of Radix |
| Font | Geist (built into shadcn) | Via shadcn preset |
| Icons | Huge Icons | Via shadcn preset |
| Theme | Violet / Zinc base | Via shadcn preset |
| State management | Zustand 5.x | SSE-driven stores for tasks/epics |
| Data fetching | TanStack Query 5.x | Settings, providers, memory |
| Build tool | Vite | Tauri's recommended frontend bundler |
| Package manager | pnpm | Consistent with old project |
| Auto-update | tauri-plugin-updater | GitHub Releases + CI-generated JSON manifest |

### Scaffolding Command
```bash
pnpm dlx shadcn@latest create --preset "https://ui.shadcn.com/init?base=base&style=mira&baseColor=zinc&theme=violet&iconLibrary=hugeicons&font=geist&menuAccent=subtle&menuColor=default&radius=small&template=vite&rtl=false" --template vite
```

## Architecture

```
Djinn Desktop (Tauri main process — Rust)
├── Spawns: djinn-server binary (child process)
│   ├── MCP server on localhost:PORT
│   ├── SSE endpoint at /events
│   └── Health check at /health
│
└── Webview (OS-native)
    └── React 19 App (Vite-bundled)
        ├── Zustand stores ← SSE stream (tasks, epics, real-time state)
        ├── TanStack Query ← MCP HTTP calls (settings, providers)
        ├── Pages: Kanban, Roadmap, Settings, Onboarding
        └── Components: shadcn/ui (Base UI + Tailwind)
```

### Communication Model
- **Server → Desktop**: SSE stream with full-entity events (task created/updated/deleted, epic state changes). Desktop updates Zustand stores directly from event payloads — no follow-up reads needed.
- **Desktop → Server**: MCP tool calls over HTTP (task_create, task_update, settings_save, etc.)
- **Server lifecycle**: Tauri main process spawns server binary, passes Clerk JWT + config, monitors health via system_ping.

### Execution Model (New Server)
- Tasks belong to epics (no phases)
- Coordinator dispatches Goose agents to ready tasks
- Task completion → auto-merge to main
- Epic review when all tasks in epic are complete
- No stacked branches, no phase DAGs

## Success Metrics

- Bundle size under 15 MB (vs old 80-150 MB Electron)
- RAM at idle under 60 MB (vs old 200-400 MB)
- All V1 features functional: kanban, roadmap, settings, onboarding
- Auto-update working via GitHub Releases
- Server spawn + health check under 3 seconds
- SSE-driven real-time updates with no polling

## Constraints

- Must work with the new Rust server's MCP tool interface (no direct DB access from desktop)
- Tauri's OS webview means testing across WebKit (macOS), WebView2 (Windows), WebKitGTK (Linux)
- Clerk JWT auth flow carried from old architecture (ADR-004)
- Server binary bundled in app resources (same pattern as old Electron app)
- pnpm as package manager (monorepo consistency)

## Reference

- Old desktop codebase: `/home/fernando/git/cli` (Electron + Go, reference for UI patterns and components)
- New server: `/home/fernando/git/djinnos/server` (Rust, 7 ADRs, MCP tools, SSE events)
- Server ADRs: ADR-001 (Rust), ADR-002 (rusqlite), ADR-004 (Clerk JWT), ADR-005 (server lifecycle), ADR-007 (git sync), ADR-008 (Goose agents)

## Relations
- [[V1 Requirements]] — detailed requirement breakdown
- [[Roadmap]] — phased delivery plan
- [[Stack Research]] — Tauri 2.x, Vite, React ecosystem research
- [[Features Research]] — desktop app feature patterns
- [[Architecture Research]] — Tauri architecture patterns and server integration
- [[Pitfalls Research]] — common desktop app rebuild mistakes