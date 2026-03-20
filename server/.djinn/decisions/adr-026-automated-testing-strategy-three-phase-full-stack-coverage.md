---
title: "ADR-026: Automated Testing Strategy — Three-Phase Full-Stack Coverage"
type: adr
tags: ["adr","testing","ci","playwright","mcp","contract","e2e"]
---


# ADR-026: Automated Testing Strategy — Three-Phase Full-Stack Coverage

**Status:** Accepted (Phase 1 complete, 2026-03-12)
**Date:** 2026-03-08
**Related:** [[ADR-019: MCP as Single API and Typed Tool Schemas]]

---

## Context

Djinn OS has two main components — a Rust server (Axum + Goose + MCP) and a Tauri 2 desktop app (React + TypeScript). The desktop communicates with the server exclusively through MCP over HTTP (port 8372), as established in ADR-019.

Current test coverage is minimal:
- **Server:** ~50 tests (task repo ~25, commands ~5, git actor ~5, server integration ~11, memory params ~5)
- **Desktop:** Zero automated tests
- **Cross-component:** No integration or E2E tests
- **CI:** No automated pipeline

The system is growing extensively. Without automated testing, every feature addition risks breaking existing behavior. The MCP contract between desktop and server is the critical boundary — changes on either side can silently break the other.

Additionally, AI-assisted development (Claude Code working in-session) needs to verify changes automatically. The developer workflow requires that `cargo test` and `pnpm test` produce meaningful, fast results so that AI and human developers can iterate with confidence.

### What Spotify, Stripe, and Others Do

Companies with similar architectures (native clients + API backends) use a layered testing approach:

1. **API contract testing** — Schema snapshots and contract tests ensure the API surface doesn't break clients (Stripe's approach)
2. **Component testing** — UI components tested in isolation without the full app runtime (Spotify uses Storybook + Testing Library)
3. **E2E testing** — Headless browser/app automation for critical user flows (Playwright, WebDriver)
4. **CI enforcement** — All of the above runs on every PR, blocking merge on failure

Djinn already has the foundation for approach #1 (MCP schema snapshot exists at `tests/fixtures/mcp_tools_schema_snapshot.json`) and #2 (Storybook is configured in the desktop app). The gap is filling in actual tests and wiring them into a pipeline.

## Decision

Implement automated testing in three phases, each building on the previous. Each phase is independently valuable — later phases enhance but don't gate earlier ones.

### Phase 1: Server Test Foundation (~195 tests)

**Goal:** Every server module has meaningful test coverage. `cargo test` catches regressions automatically.

#### 1.1 Expand Test Helpers

Before writing tests, expand `src/test_helpers.rs` with shared fixtures:

```rust
create_test_db()                              // exists
create_test_app()                             // exists
create_test_project(db) -> Project            // NEW
create_test_epic(db, project_id) -> Epic      // NEW
create_test_task(db, project_id, epic_id?) -> Task  // NEW
create_test_session(db, project_id, task_id) -> SessionRecord  // NEW
create_test_note(db, project_id) -> Note      // NEW
```

Promote `mcp_call_tool()` and `extract_tool_result_payload()` from `server/mod.rs` tests to shared test helpers.

#### 1.2 MCP Tool Contract Tests (~80 tests)

Every MCP tool gets success-shape and error-shape tests. These are the desktop↔server contract — the highest-value tests.

**P0 — Task & Epic tools (30 tools, ~50 tests):**
- `task_create` success/error shapes
- `task_show` found/not-found
- `task_list` with filters (status, priority, label, text, pagination)
- `task_transition` all valid actions + invalid transitions
- `task_claim` success/empty board
- `task_count` plain + grouped (by status, priority, issue_type, epic)
- `task_comment_add`, `task_activity_list`
- `task_ready`, `board_health`, `board_reconcile`
- `task_blockers_list`, `task_blocked_list`, `task_memory_refs`
- `task_update` partial field updates
- `epic_create`, `epic_show` with task counts
- `epic_list` + filters, `epic_update`
- `epic_close`/`epic_reopen`, `epic_delete` cascade
- `epic_tasks`, `epic_count` + grouped

**P1 — Session, Memory, Project tools (~20 tests):**
- `session_list`, `session_active`, `session_show`
- `memory_write`, `memory_read` (by permalink + title), `memory_search` FTS
- `memory_edit` operations, `memory_move`, `memory_delete`
- `memory_graph`, `memory_recent`
- `project_add`/`project_remove`, `project_list`
- `project_config_get`/`set`, `project_commands_get`/`set`

**P2 — Credential, Settings, System tools (~10 tests):**
- `credential_set`, `credential_list`, `credential_delete`
- `settings_get`/`set`/`reset`, model priority validation
- `system_ping`

#### 1.3 Task State Machine Tests (~25 tests)

The task status transitions are the most complex logic in the system.

**Valid transitions to test:**
```
Draft/Backlog → Open (action: open/pm_approve)
Open → InProgress (action: start)
InProgress → NeedsTaskReview (action: request_review)
NeedsTaskReview → InTaskReview (auto)
InTaskReview → Closed (action: approve)
InTaskReview → InProgress (action: request_changes)
Any → Closed (action: close)
Closed → Open (action: reopen)
Any → Any (action: user_override + target_status)
```

**Tests:**
- Every valid transition produces correct status + timestamps
- Every invalid transition returns an error (not silent failure)
- `reopen_count` increments on reopen
- `closed_at` set on close, cleared on reopen
- Activity log entry created for each transition
- Actor role validation (worker can't approve, only task_reviewer can)

#### 1.4 Repository Layer Tests (~60 tests)

Fill coverage gaps in the database layer:

| Repository | Existing | Tests to Add |
|-----------|----------|-------------|
| task/ | 25+ | Filtered list edge cases, claim atomicity, count grouping, memory_refs CRUD |
| epic/ | 0 | Full CRUD, cascade delete, task counts, filtered list, close/reopen |
| session/ | 0 | Create, lifecycle, continuation chain, active listing, token recording |
| note/ | 0 | CRUD, FTS search ranking, link graph, health report |
| project/ | 0 | CRUD, config fields, command validation |
| credential/ | 0 | Set/get/list/delete, provider_id filtering |
| settings/ | 0 | Get/set/delete, JSON deserialization |

#### 1.5 Event Emission Tests (~20 tests)

Verify every mutation emits the correct `DjinnEvent` on the broadcast channel:

- Task create/update/status change/close/reopen → corresponding events
- Epic create/update/close/reopen → corresponding events
- Session start/end → corresponding events
- Note CRUD → corresponding events

Pattern: subscribe to broadcast channel, perform mutation, assert event received with correct payload.

#### 1.6 Integration Smoke Tests (~10 tests)

Full HTTP round-trips through the Axum router:
- Health endpoint returns OK
- MCP initialize handshake succeeds
- MCP `tools/list` returns all registered tools
- MCP tool call returns success response
- MCP tool call with bad params returns error response
- SSE event stream connects and receives events
- Schema snapshot regression (already exists, keep it)

#### File Organization

```
src/
  test_helpers.rs              ← expanded shared fixtures
  server/mod.rs                ← existing integration tests (keep + expand)
  db/repositories/
    task/tests.rs              ← existing (expand)
    epic/tests.rs              ← NEW
    session/tests.rs           ← NEW
    note/tests.rs              ← NEW
    project/tests.rs           ← NEW
    credential/tests.rs        ← NEW
    settings/tests.rs          ← NEW
  mcp/tools/
    task_tools/tests.rs        ← NEW
    epic_tools/tests.rs        ← NEW
    session_tools/tests.rs     ← NEW
    memory_tools/tests.rs      ← expand existing
    project_tools/tests.rs     ← NEW
    credential_tools/tests.rs  ← expand existing
    settings_tools/tests.rs    ← NEW
    system_tools/tests.rs      ← NEW
  actors/
    git/tests.rs               ← existing (keep)
    slot/pool/tests.rs         ← existing (keep)
```

---

### Phase 2: Desktop Testing + Cross-Component (~110 tests)

**Depends on:** Phase 1 server tests as a safety net.

#### 2.1 Vitest Setup

Add Vitest to the desktop app for unit and component testing:

```
pnpm add -D vitest @testing-library/react @testing-library/jest-dom jsdom
```

**What to test:**
- Zustand stores (state management logic, ~20 tests)
- Utility functions and data transformers (~15 tests)
- Custom hooks (~10 tests)

#### 2.2 Component Tests with Testing Library (~40 tests)

Test React components in isolation using jsdom — no Tauri window needed:

- Task views: card rendering, status badges, priority display, filter controls
- Epic views: progress bars, task count display
- Session views: active indicators, history list
- Memory/notes: search results, link graph display
- Settings: forms, validation feedback
- Shared components: buttons, modals, dropdowns, toasts, loading states

Storybook is already configured — use Storybook stories as component test fixtures where applicable.

#### 2.3 Playwright + Tauri WebDriver for E2E (~15 tests)

Tauri supports WebDriver testing via `tauri-driver`. Playwright connects headlessly.

```
1. Build desktop in debug mode: cargo tauri build --debug
2. Start tauri-driver (WebDriver bridge)
3. Playwright drives the app — clicks, types, asserts DOM state
4. Test results are pass/fail + error messages (readable by AI)
```

**Critical path E2E tests (P0):**
- App launches and shows main view
- Create project → appears in list
- Create task → appears on board
- Transition task through states → UI updates
- Create epic → assign tasks → shows counts

**Core flow tests (P1):**
- Search/filter tasks → correct results
- Create and search notes
- Settings page → change settings → persists

**Visual regression (optional):**
- Playwright takes screenshots at key states
- Compares against baselines
- Screenshot diffs are images — Claude Code can inspect them visually (multimodal)

#### 2.4 Desktop ↔ Server Integration (~10 tests)

Test actual MCP communication without mocking:
1. Start server in test mode (`cargo run -- --port 0` for random port)
2. Desktop test hits real MCP endpoints
3. Verify round-trip: action in UI → server processes → UI reflects result

#### File Organization

```
desktop/
  vitest.config.ts
  src/
    stores/*.test.ts           ← Zustand store tests
    components/*.test.tsx      ← Component tests
    hooks/*.test.ts            ← Custom hook tests
    lib/*.test.ts              ← Utility tests
  e2e/
    setup.ts                   ← Tauri WebDriver config
    tasks.spec.ts
    epics.spec.ts
    notes.spec.ts
    settings.spec.ts
```

---

### Phase 3: CI/CD Pipeline + Contract Enforcement (~27 tests)

**Depends on:** Phase 1 + Phase 2 tests exist to run.

#### 3.1 GitHub Actions Pipeline

```yaml
# Runs on every PR
jobs:
  server-tests:
    - cargo test
    - cargo clippy -- -D warnings
    - cargo fmt --check

  desktop-tests:
    - pnpm vitest run
    - pnpm tsc --noEmit
    - pnpm lint

  schema-contract:
    - Build server, start it, dump MCP schema
    - Compare against committed snapshot
    - Fail if schema changed without updating snapshot

  e2e-tests:
    - Build server + desktop
    - Run Playwright E2E suite
    - Upload failure screenshots as artifacts
```

#### 3.2 MCP Schema Contract Testing (~10 tests)

The MCP tool schema (from `tools/list`) is the API contract between desktop and server.

**Breaking changes (CI fails):**
- Removing a tool
- Removing a required field from output
- Changing field types or renaming fields

**Non-breaking (CI passes):**
- Adding new tools
- Adding optional fields to output
- Adding optional parameters to input

**TypeScript type enforcement:** Desktop already has `pnpm mcp:types` for generated TS types from server schema. CI verifies types are not stale — forces desktop to update when server changes.

#### 3.3 Test Fixture Factories

Realistic seed data for consistent test environments:

```rust
seed_full_project(db)     // 1 project, 3 epics, 15 tasks, sessions, notes, credentials
seed_minimal(db)          // 1 project, 1 epic, 1 task
seed_empty_project(db)    // project with nothing
seed_blocked_chain(db)    // A blocks B blocks C
seed_large_board(db)      // 100+ tasks for pagination
```

Mirrored in TypeScript for desktop component tests.

#### 3.4 Coverage Tracking

- Server: `cargo-tarpaulin` with HTML + Lcov output
- Desktop: Vitest v8 coverage provider
- Track trends, not percentages — direction matters more than targets

#### 3.5 Release Smoke Tests (~7 tests)

Pre-release automated checklist:
1. Server binary builds and starts cleanly
2. Database migrations run on fresh DB
3. Desktop builds without errors
4. Sidecar sync produces valid binary
5. MCP handshake succeeds
6. At least one tool call round-trips
7. SSE stream connects

#### 3.6 Performance Baselines (Future)

For when the system scales:
- MCP throughput (tool calls/second)
- SQLite write contention under concurrent task creation
- SSE scalability with multiple connected clients
- Agent session limits before degradation

Tools: `criterion` (Rust benchmarks), `k6` (HTTP load testing)

---

### How Claude Code Tests the Full Stack In-Session

After all three phases, a typical AI-assisted development session works like this:

```
┌─────────────────────────────────────────────────────────┐
│  Developer: "Add priority filter to the task board"     │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  Layer 1: Server (Claude owns completely)                │
│    → Write filter param in task_list tool                │
│    → cargo test              ✅ contract tests pass      │
│    → curl localhost:8372     ✅ verify response shape    │
│                                                         │
│  Layer 2: Desktop Build (Claude owns completely)         │
│    → Write filter component                             │
│    → pnpm tsc                ✅ types compile            │
│    → pnpm vitest             ✅ component renders,       │
│                                 correct API params      │
│                                                         │
│  Layer 3: Desktop UI (Claude via Playwright)             │
│    → pnpm playwright test    ✅ opens app headlessly,    │
│                                 clicks filter,          │
│                                 verifies list updates   │
│    → screenshot diff         ✅ Claude inspects visual   │
│                                 changes (multimodal)    │
│                                                         │
│  Layer 4: All green → ready for human review             │
└─────────────────────────────────────────────────────────┘
```

**What Claude Code can do:**
- Start server in background (`cargo run &`)
- Hit MCP endpoints via HTTP
- Run `cargo test`, `pnpm vitest`, `pnpm playwright test`
- Read all test output (pass/fail/errors)
- Inspect screenshot diffs (multimodal image reading)

**What requires a human:**
- Visual polish, animations, "does it feel right"
- Cross-device testing (mobile, different OS)
- Accessibility testing beyond automated checks

---

### Makefile Integration

```makefile
# Server
test:            cargo test
test-quick:      cargo test --lib
test-coverage:   cargo tarpaulin --out Html

# Desktop
test-desktop:    cd desktop && pnpm vitest run
test-e2e:        cd desktop && pnpm playwright test

# Full stack
test-all:        cargo test && cd desktop && pnpm vitest run && pnpm playwright test
```

## Consequences

### Positive

- **~332 tests** across server, desktop, and integration layers
- Every MCP tool has a contract test — desktop↔server changes are caught automatically
- Task state machine fully tested — the most complex logic in the system
- Claude Code can verify changes in-session without human intervention
- CI pipeline blocks broken PRs before merge
- Schema contract prevents accidental API drift
- Foundation scales — new features get tested by following established patterns

### Negative

- Initial investment to write ~332 tests before they start paying back
- Playwright + Tauri WebDriver setup has complexity (tauri-driver, WebDriver protocol)
- E2E tests are inherently slower and more brittle than unit tests
- Screenshot diffing requires baseline maintenance
- Test fixtures must stay in sync with schema migrations

### Risks

1. **Test maintenance burden** — Mitigated: test at boundaries (MCP contract), not implementation details. Tests survive refactors.
2. **E2E flakiness** — Mitigated: keep E2E suite small (~15 tests), use component tests for most UI coverage.
3. **Fixture drift** — Mitigated: fixtures use the same `create_test_db()` with migrations, so they evolve with the schema.
4. **Over-testing** — Mitigated: priority tiers (P0/P1/P2) focus effort on highest-value tests first.

## Relations

- [[ADR-019: MCP as Single API and Typed Tool Schemas]] — MCP is the contract being tested
- [[Roadmap]] — testing infrastructure supports all future phases
