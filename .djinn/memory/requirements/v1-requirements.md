---
tags:
    - planning
    - requirements
title: V1 Requirements
type: requirement
---
# V1 Requirements — Djinn Desktop

## Requirement Categories

| Prefix | Domain |
|--------|--------|
| SHELL | Tauri shell, sidecar, lifecycle |
| AUTH | Authentication (Clerk JWT) |
| UI | Core UI framework, layout, theming |
| KANBAN | Kanban board |
| ROAD | Roadmap view |
| SETTINGS | Settings and configuration |
| ONBOARD | Onboarding wizard |
| UPDATE | Auto-updater and distribution |
| SSE | Server-Sent Events integration |
| CI | Build pipeline and release |

---

## SHELL — Tauri Shell & Server Lifecycle

| ID | Requirement | Class |
|----|------------|-------|
| SHELL-01 | Tauri 2.x app with Vite + React 19 frontend | v1 |
| SHELL-02 | Server binary bundled as sidecar via `externalBin` with platform-triple naming | v1 |
| SHELL-03 | Daemon discovery: read `~/.djinn/daemon.json` (pid, port, started_at), check PID alive, connect if running | v1 |
| SHELL-04 | Daemon start: if no running server found, spawn server detached (not child process), wait for lockfile to appear, then connect | v1 |
| SHELL-05 | Desktop closing does NOT stop the server daemon | v1 |
| SHELL-06 | Health check: verify server reachable via `/health` before showing main UI | v1 |
| SHELL-07 | macOS close handling: `CloseRequested` → explicit `app.exit(0)` (closing last window doesn't quit on macOS) | v1 |
| SHELL-08 | Single instance enforcement via `tauri-plugin-single-instance` (focus existing window on duplicate launch) | v1 |
| SHELL-09 | `#[tauri::command] get_server_port()` exposed to frontend (reads from daemon.json or spawn result) | v1 |
| SHELL-10 | CSP configured: `connect-src` allows `http://127.0.0.1:{port}` for fetch and SSE | v1 |
| SHELL-11 | Window starts hidden (`visible: false`), shown after daemon discovery + health check passes | v1 |
| SHELL-12 | Loading/splash state shown while discovering/starting server | v1 |
| SHELL-13 | Error state if server fails to start after retries (with "Retry" action) | v1 |
| SHELL-14 | System tray with status indicator (running/error), show/hide window, quit | v2 |


## AUTH — Clerk JWT Authentication

| ID | Requirement | Class |
|----|------------|-------|
| AUTH-01 | Desktop handles Clerk sign-in via system browser + deep link callback (`djinn://auth/callback`) | v1 |
| AUTH-02 | Deep link registration via `tauri-plugin-deep-link` for `djinn://` scheme | v1 |
| AUTH-03 | Desktop passes initial token to server (at spawn via CLI arg, or via one-time HTTP POST if server already running) | v1 |
| AUTH-04 | Server writes/refreshes Clerk JWT in `~/.djinn/daemon.json` alongside pid/port — server owns the refresh cycle | v1 |
| AUTH-05 | Desktop reads token from `daemon.json` on reconnect (no OS keychain, no Stronghold) | v1 |
| AUTH-06 | `daemon.json` file permissions: `chmod 600` (user-only read/write) | v1 |
| AUTH-07 | Auth error handling: if token expired and server refresh fails, desktop prompts re-authentication via Clerk | v1 |
| AUTH-08 | Sign-out: desktop tells server to clear token from `daemon.json`, returns to sign-in state | v1 |


## UI — Core UI Framework

| ID | Requirement | Class |
|----|------------|-------|
| UI-01 | shadcn/ui with Base UI primitives, Mira style, Geist font, Huge Icons, violet/zinc theme | v1 |
| UI-02 | Dark theme only (no light mode toggle in v1) | v1 |
| UI-03 | Persistent left sidebar navigation (240-280px, collapsible to icon-only 48-64px) | v1 |
| UI-04 | Sidebar sections: Kanban, Roadmap (primary); Settings (pinned bottom) | v1 |
| UI-05 | Command palette (Cmd+K / Ctrl+K) via shadcn `<Command>` (cmdk) | v1 |
| UI-06 | Toast notifications via Sonner (ships with shadcn) | v1 |
| UI-07 | Keyboard shortcuts: Cmd+K (palette), Cmd+, (settings), Esc (dismiss), Cmd+/ (toggle sidebar) | v1 |
| UI-08 | Skeleton loaders for content areas, spinners for button-triggered actions | v1 |
| UI-09 | Error states: inline error with retry for data-fetch failures, toast for transient errors | v1 |
| UI-10 | Empty states with action prompt for first-time/no-data views | v1 |
| UI-11 | Responsive layout that handles window resize gracefully | v1 |
| UI-12 | Light/dark theme toggle with system-sync | v2 |
| UI-13 | Resizable panels (drag-to-resize sidebar, split panes) | v2 |

## KANBAN — Kanban Board

| ID | Requirement | Class |
|----|------------|-------|
| KANBAN-01 | Read-only kanban board with columns by task status (open, in_progress, needs_review, approved, closed) | v1 |
| KANBAN-02 | Tasks displayed as cards showing: title, priority badge, epic color/emoji, owner | v1 |
| KANBAN-03 | Tasks grouped by epic within columns (epic header with collapse/expand) | v1 |
| KANBAN-04 | Real-time updates: task cards move between columns as SSE events arrive | v1 |
| KANBAN-05 | Click task card → task detail panel/modal showing full description, acceptance criteria, design, activity | v1 |
| KANBAN-06 | Filter by: epic, priority, owner, text search | v1 |
| KANBAN-07 | Task count per column in column header | v1 |
| KANBAN-08 | Project selector: switch between registered projects, kanban shows selected project's tasks | v1 |
| KANBAN-09 | Drag-and-drop task reordering and status changes | v2 |

## ROAD — Roadmap View

| ID | Requirement | Class |
|----|------------|-------|
| ROAD-01 | Roadmap view showing all epics with their tasks grouped underneath | v1 |
| ROAD-02 | Epic progress bar: percentage of tasks in closed status | v1 |
| ROAD-03 | Epic cards showing: emoji, title, color, task count (done/total), progress bar | v1 |
| ROAD-04 | Tasks under each epic listed with status badge and title | v1 |
| ROAD-05 | Real-time updates: progress bars and status badges update as SSE events arrive | v1 |
| ROAD-06 | Click epic → expand/collapse task list | v1 |
| ROAD-07 | Click task → same detail panel as kanban (shared component) | v1 |
| ROAD-08 | Dependency graph visualization with @xyflow/react showing epic relationships | v2 |

## SETTINGS — Settings & Configuration

| ID | Requirement | Class |
|----|------------|-------|
| SETTINGS-01 | Settings page with left sidebar categories + right content pane | v1 |
| SETTINGS-02 | Providers section: list configured LLM providers with status indicator (connected/error/unconfigured) | v1 |
| SETTINGS-03 | API key management: masked display (first 4 + last 4 chars), reveal toggle, copy, test connection, delete | v1 |
| SETTINGS-04 | Add provider: select from catalog, enter API key, validate inline before saving | v1 |
| SETTINGS-05 | Provider credentials stored via server's encrypted vault (MCP `credential_*` tools) | v1 |
| SETTINGS-06 | Project settings: registered projects list, add/remove project | v1 |
| SETTINGS-07 | Per-project git config: target branch, auto-merge toggle | v1 |
| SETTINGS-08 | General settings: default project selection | v1 |
| SETTINGS-09 | Auto-save on change (no explicit save button), brief "Saved" toast confirmation | v1 |
| SETTINGS-10 | Agent configuration: per-model session limits, default model selection | v2 |
| SETTINGS-11 | Keyboard shortcuts viewer/remapper | v2 |

## ONBOARD — Onboarding Wizard

| ID | Requirement | Class |
|----|------------|-------|
| ONBOARD-01 | First-run detection: show wizard if no projects registered and no provider configured | v1 |
| ONBOARD-02 | Step 1: Server connection check (automated, show spinner → success indicator) | v1 |
| ONBOARD-03 | Step 2: Clerk sign-in (opens system browser, waits for deep link callback) | v1 |
| ONBOARD-04 | Step 3: Provider setup (select provider, enter API key, validate inline) | v1 |
| ONBOARD-05 | Step 4: Project setup (select directory, register project) | v1 |
| ONBOARD-06 | Step indicator showing current step and total (e.g., "Step 2 of 4") | v1 |
| ONBOARD-07 | Skip available on every step (configure later in settings) | v1 |
| ONBOARD-08 | Persist partial progress (resume where left off if app closes mid-wizard) | v1 |
| ONBOARD-09 | Done state with "what's next" prompt (create first task, view kanban) | v1 |

## SSE — Server-Sent Events Integration

| ID | Requirement | Class |
|----|------------|-------|
| SSE-01 | EventSource connection to server's `/events` endpoint on startup | v1 |
| SSE-02 | Zustand store updated directly from SSE event payloads (full-entity events per server ADR-002) | v1 |
| SSE-03 | Event types handled: task created/updated/deleted, epic created/updated, project changes | v1 |
| SSE-04 | Automatic reconnection with exponential backoff on connection loss | v1 |
| SSE-05 | Connection status indicator in UI (connected/reconnecting/error) | v1 |
| SSE-06 | Last-Event-ID support for replay of missed events on reconnect | v1 |
| SSE-07 | SSE events trigger TanStack Query cache invalidation where appropriate (settings changes, provider updates) | v1 |

## UPDATE — Auto-Updater & Distribution

| ID | Requirement | Class |
|----|------------|-------|
| UPDATE-01 | `tauri-plugin-updater` configured with GitHub Releases endpoint | v1 |
| UPDATE-02 | ed25519 signing keypair generated, public key in `tauri.conf.json`, private key in CI secrets | v1 |
| UPDATE-03 | Update check on app startup, non-blocking | v1 |
| UPDATE-04 | Update available dialog: show version + release notes, confirm before installing | v1 |
| UPDATE-05 | Download with progress indicator, then install + relaunch via `tauri-plugin-process` | v1 |
| UPDATE-06 | Distribution targets: macOS DMG (arm64, x64), Linux AppImage (x64), Windows NSIS (x64) | v1 |

## CI — Build Pipeline & Release

| ID | Requirement | Class |
|----|------------|-------|
| CI-01 | GitHub Actions workflow: build Tauri app for all three platforms | v1 |
| CI-02 | `tauri-action@v0` with `includeUpdaterJson: true` to auto-generate `latest.json` | v1 |
| CI-03 | Build matrix: macOS (arm64 + x64), Linux (x64, ubuntu-22.04), Windows (x64) | v1 |
| CI-04 | Server sidecar binary built and placed in `src-tauri/binaries/` with platform-triple naming | v1 |
| CI-05 | macOS code signing + notarization (Apple Developer account, JIT entitlement) | v1 |
| CI-06 | Windows code signing via Azure Trusted Signing | v1 |
| CI-07 | Sidecar binaries individually codesigned before bundling (macOS notarization requirement) | v1 |
| CI-08 | Release draft created on GitHub with all artifacts + `latest.json` | v1 |
| CI-09 | Rust build caching via `swatinem/rust-cache` | v1 |

---

## Out of Scope

| Feature | Reason |
|---------|--------|
| Drag-and-drop kanban | Unnecessary complexity for v1; status changes via server/CLI |
| Agent monitoring / session streaming | V2; server will expose via SSE |
| Knowledge graph / docs browser | V2 feature |
| Phase editor | Phases eliminated from new server |
| Code diff viewer | V2; review handled server-side |
| Light theme / system-sync | V2; dark-only for v1 |
| Mobile support | Out of scope entirely |
| Microsoft Store distribution | Tauri doesn't support .msix/.appx |
| Multi-window support | V2 |
| Plugin/extension system | V2 |

---

## Traceability

| Requirement | Research Source |
|-------------|---------------|
| SHELL-01 through SHELL-13 | [[Architecture Research]] — sidecar pattern, startup sequence |
| SHELL-03 (dynamic port) | [[Research Summary]] — open question resolved during questioning |
| AUTH-01 through AUTH-07 | [[Architecture Research]] — Clerk JWT deep link pattern; Server ADR-004 |
| UI-01 | [[Stack Research]] — shadcn Base UI setup |
| UI-05, UI-06 | [[Features Research]] — cmdk, Sonner as table stakes |
| KANBAN-01 through KANBAN-08 | [[Features Research]] — kanban patterns (DnD removed per user decision) |
| ROAD-01 through ROAD-07 | [[Features Research]] — roadmap visualization |
| SETTINGS-01 through SETTINGS-09 | [[Features Research]] — settings UX patterns |
| ONBOARD-01 through ONBOARD-09 | [[Features Research]] — onboarding wizard patterns |
| SSE-01 through SSE-07 | [[Stack Research]], [[Architecture Research]] — SSE + Zustand pattern |
| UPDATE-01 through UPDATE-06 | [[Stack Research]] — tauri-plugin-updater, GitHub Releases |
| CI-01 through CI-09 | [[Stack Research]], [[Pitfalls Research]] — CI pipeline, signing requirements |

## Relations
- [[Project Brief]] — vision and scope
- [[Research Summary]] — cross-cutting synthesis informing these requirements
- [[Roadmap]] — phased delivery of these requirements