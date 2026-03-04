---
title: Roadmap
type: roadmap
tags: []
---

# Roadmap — Djinn Desktop

## Phase 1: Foundation
**Goal:** Tauri app boots, discovers/spawns server daemon, connects, and renders a loading state.

**Depends on:** Nothing (starting point)

**Requirements:** SHELL-01, SHELL-02, SHELL-03, SHELL-04, SHELL-06, SHELL-07, SHELL-08, SHELL-09, SHELL-10, SHELL-11, SHELL-12, SHELL-13

**Success Criteria:**
- `pnpm tauri dev` launches Tauri window
- App reads `~/.djinn/daemon.json`, connects to running server OR spawns detached server and waits
- Health check passes, window transitions from loading to empty shell
- Single instance enforced (second launch focuses existing window)
- macOS close behavior correct (app exits on last window close)

---

## Phase 2: Auth & Onboarding
**Goal:** User can sign in via Clerk and complete first-run setup.

**Depends on:** Phase 1

**Requirements:** AUTH-01, AUTH-02, AUTH-03, AUTH-04, AUTH-05, AUTH-06, AUTH-07, AUTH-08, ONBOARD-01 through ONBOARD-09

**Success Criteria:**
- Clerk sign-in opens system browser, deep link callback captures token
- Token passed to server, written to `daemon.json`
- On reconnect, desktop reads token from file without prompting
- Onboarding wizard: server check → sign-in → provider setup → project setup
- Skip available on every step, partial progress persisted
- After onboarding, user lands on kanban view

---

## Phase 3: Core UI & SSE
**Goal:** App shell is complete with sidebar nav, theming, and real-time server connection.

**Depends on:** Phase 1

**Requirements:** UI-01 through UI-11, SSE-01 through SSE-07

**Success Criteria:**
- shadcn/Base UI with Mira style, Geist font, dark theme renders correctly
- Sidebar navigation works (Kanban, Roadmap, Settings)
- Command palette opens on Cmd+K with navigation commands
- Toasts display on actions
- SSE connection established, Zustand stores update from server events
- Connection status indicator shows connected/reconnecting/error
- Skeleton loaders shown while data loads

---

## Phase 4: Kanban & Roadmap
**Goal:** User can view task state and epic progress in real-time.

**Depends on:** Phase 3

**Requirements:** KANBAN-01 through KANBAN-08, ROAD-01 through ROAD-07

**Success Criteria:**
- Kanban shows tasks in status columns with epic grouping
- Task cards show title, priority, epic color/emoji, owner
- Click card opens detail panel with full task info
- Filters work (epic, priority, owner, text)
- Roadmap shows epics with progress bars and task lists
- Cards and progress update in real-time as SSE events arrive
- Project selector switches between registered projects

---

## Phase 5: Settings
**Goal:** User can manage providers, credentials, and project configuration.

**Depends on:** Phase 3

**Requirements:** SETTINGS-01 through SETTINGS-09

**Success Criteria:**
- Settings page with category sidebar (Providers, Projects, General)
- Provider list with status indicators
- API key CRUD with masked display, test connection, inline validation
- Project list with add/remove
- Per-project git config (target branch, auto-merge)
- Auto-save on change with toast confirmation

---

## Phase 6: Distribution (Public Repo)
**Goal:** CI, code signing, release automation, and updater flows are tracked in the public repo roadmap, not in this desktop planning board.

**Depends on:** N/A in this repo

**Requirements:** N/A in this repo

**Success Criteria:**
- Desktop repo focuses on runtime, UX, and local integration scope
- CI/release work is owned and tracked in the public repo

---

## Parallel Execution Notes

- **Phase 3** can run in parallel with **Phase 2** (independent)
- **Phase 4 and Phase 5** can run in parallel (both depend on Phase 3, independent of each other)
- **Phase 6** is tracked in the public repo roadmap (not executed from this desktop board)
- **Phase 2** depends on Phase 1 only

```
Phase 1 (Foundation)
├── Phase 2 (Auth & Onboarding)
└── Phase 3 (Core UI & SSE)
    ├── Phase 4 (Kanban & Roadmap)
    └── Phase 5 (Settings)

Phase 6 (Distribution/Public Repo) tracked externally
```

## Relations
- [[Project Brief]] — vision and scope
- [[V1 Requirements]] — full requirement breakdown
- [[Research Summary]] — recommendations informing phase order