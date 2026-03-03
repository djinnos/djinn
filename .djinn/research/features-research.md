---
tags:
    - research
    - features
title: Features Research
type: research
---
# Features Research — Desktop Dev Tool UX Patterns 2026

## Kanban Board

**Use `@dnd-kit/react` + `@dnd-kit/sortable` + `@dnd-kit/modifiers`.**

- `@dnd-kit/react` v0.3.x (late 2025) is the forward-looking API for React 19
- Known minor issue: `"use client"` directive on `DragDropProvider` — easily handled
- `pragmatic-drag-and-drop` (Atlassian): partial React 19 support, untested touch — skip
- `react-beautiful-dnd`: **deprecated**. `hello-pangea/dnd` is the community fork but heavier
- Use `closestCorners` collision detection for multi-column kanban
- Use `DragOverlay` for smooth drag preview

## Roadmap / Progress Visualization

**Use `@xyflow/react` (React Flow) for dependency/relationship graphs. Use Tremor for progress bars and metric cards.**

- React Flow: still the de-facto standard for node graphs in 2026
- Layout algorithms: Dagre (fast, simple), ELK.js (configurable, heavier)
- Tremor: built on Recharts + Tailwind, ships fast for dashboard components
- For simple milestone timelines: CSS grid, no library needed

## SSE Consumption

**Custom `useEventSource` hook (~40 lines) + React Query cache invalidation.**

- Store `EventSource` in `useRef`, not `useState` (prevents re-renders)
- Exponential backoff on `onerror` (native EventSource only auto-reconnects on network drop, not error codes)
- Server must send `id:` fields for Last-Event-ID replay on reconnect
- Server must send `Access-Control-Allow-Origin: *` for Tauri webview CORS
- Alternative: `reconnecting-eventsource` npm package as drop-in wrapper

Two valid patterns:
1. **Direct hook state** — simple, good for v1
2. **React Query + SSE invalidation** — SSE events call `queryClient.setQueryData()` or `invalidateQueries()`. Recommended for multi-feature apps.

## Onboarding Wizard

**5 steps max. Skip always available. Persist partial progress.**

Prescriptive flow:
1. Server detection / connection check (automated spinner)
2. Provider configuration (select provider, enter API key, validate inline)
3. Project workspace setup (name, directory, preferences)
4. Quick-start action (create first task / connect first repo)
5. Done state with "what's next"

Rules: validate inline not on submit, make steps feel fast, deep-link to docs.

## Settings / Preferences

**Left sidebar categories + right content pane. Auto-save everything.**

Categories: General, Providers, Project, Appearance, Advanced

API Key UX:
- Mask by default (first 4 + last 4 chars)
- Reveal toggle (eye icon, 10s auto-hide)
- "Test Connection" button with inline success/error
- Store in OS keychain via `tauri-plugin-stronghold`

## System Tray

**Minimal menu: Show Window, Server Status, Pause/Resume, New Task, Preferences, Quit.**

- Tray icon encodes state (colored dot: running/paused/error)
- Left-click = show/hide window
- Right-click = context menu
- Under 10 items total
- macOS: template images for light/dark menu bar

## Table Stakes UX (v1)

| Feature | Library/Approach |
|---------|-----------------|
| Dark mode (system-sync) | `prefers-color-scheme` + manual override |
| Command palette (Cmd+K) | `cmdk` via shadcn/ui `<Command>` |
| Toast notifications | Sonner (ships with shadcn/ui) |
| Keyboard shortcuts | Core set: Cmd+K, Cmd+,, Cmd+N, Cmd+W, Cmd+/, Esc |
| Persistent sidebar | 240-280px, collapsible to icon-only |
| Error states | Inline error + retry for data-fetch; toast for transient |
| Loading states | Skeleton loaders for content; spinners for button actions |
| Accessible focus | Logical tab order, modal focus trap |

### Nice-to-Have (v1 stretch)
- Inline search/filter on kanban
- Resizable panels
- Activity/audit log view

### Defer to v2
- Remap keyboard shortcuts
- Themes beyond light/dark
- Plugin/extension system
- Multi-window support

## Relations
- [[Project Brief]] — project context
- [[Stack Research]] — technology choices
- [[Architecture Research]] — component architecture
- [[Pitfalls Research]] — UX anti-patterns to avoid