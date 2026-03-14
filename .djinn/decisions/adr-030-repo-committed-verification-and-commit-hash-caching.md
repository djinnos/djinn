---
title: "ADR-030: Repo-Committed Verification and Commit-Hash Caching"
type: adr
tags: ["adr","verification","setup","caching","observability","dx"]
---

# ADR-030: Repo-Committed Verification and Commit-Hash Caching

## Status: Draft

Date: 2026-03-13

## Related

- [[ADR-014]]: Project Setup & Verification Commands (SUPERSEDED by this ADR)
- [[ADR-009]]: Simplified Execution
- [[ADR-022]]: Outcome-Based Session Validation

## Context

The current verification system has four structural problems:

### 1. Static commands can't evolve with the code

Setup and verification commands are stored in the Djinn database as project config. Every task branch runs the same commands as main. A task that restructures the project — adding cargo workspaces, splitting a monorepo, changing the build system — will fail verification because the OLD commands run against the NEW code structure. The developer who made the change can't update the verification without pausing execution, calling `project_commands_set`, and hoping no other tasks break.

### 2. No caching — the same work runs 4+ times per task

Today there are 5 call sites that run the same setup/verification commands:

| # | Call Site | Commands | Trigger |
|---|-----------|----------|---------|
| 1 | Project healthcheck | setup + verification | `execution_start` |
| 2 | Worker setup | setup only | Before agent session |
| 3 | Post-worker verification | setup + verification | Worker completes → `verifying` |
| 4 | Pre-merge gate | setup + verification | PM/reviewer approval |
| 5 | Commands validation | setup + verification | `project_commands_set` |

Sites 1, 3, and 4 run identical commands. Sites 3→4 are particularly wasteful: post-worker verification passes, then the pre-merge gate re-runs the exact same verification on the same commit. A `cargo test` suite that takes 60 seconds runs 3 times for a single task lifecycle.

### 3. No visibility into what's happening

When execution starts, a healthcheck runs setup+verification commands on main. The desktop UI shows nothing — no progress, no step names, no output. If it fails, the project stays unhealthy with no way to see why. The same applies to task setup and verification: commands run silently, and failures only surface as task state transitions with truncated error messages in the activity log.

### 4. Healthcheck cache doesn't invalidate properly

The project healthcheck result is stored in an in-memory `unhealthy_projects` HashMap. When a developer fixes a broken build on main, restarting execution re-runs the healthcheck — but the old error persists if the temporary `_health_check` worktree has issues. There's no connection between "main has new commits" and "we should re-check."

### 5. Config isn't shared across teammates

Setup and verification commands live in each developer's local Djinn database. A new teammate cloning the repo has no commands configured. There's no way to version-control the verification pipeline alongside the code it validates.

## Decision

### Part 1: `.djinn/settings.json` — Repo-committed verification config

Move setup and verification commands from the database into a git-tracked file at `.djinn/settings.json` in each project repository.

**File format:**

```json
{
  "setup": [
    { "name": "install", "command": "cargo build", "timeout_secs": 300 }
  ],
  "verification": [
    { "name": "clippy", "command": "cargo clippy -- -D warnings", "timeout_secs": 120 },
    { "name": "test", "command": "RUSTFLAGS=\"-D warnings\" cargo test", "timeout_secs": 300 }
  ]
}
```

**Schema:**

```typescript
interface DjinnSettings {
  setup?: CommandSpec[];
  verification?: CommandSpec[];
}

interface CommandSpec {
  name: string;        // Human-readable label
  command: string;     // Shell command (executed via `sh -c`)
  timeout_secs?: number; // Default: 300
}
```

**Key properties:**

- **Branch-specific**: Each branch carries its own settings. A task that restructures the project updates `.djinn/settings.json` in the same commit. The new verification runs on the new code.
- **Shared via git**: All teammates get the same verification by cloning the repo. No manual DB setup.
- **Self-healing**: A task that breaks the build can also fix the verification commands, and verification runs the version from that commit.
- **Backward compatible**: If `.djinn/settings.json` doesn't exist, fall back to DB-stored commands (migration path). If both exist, the file wins.

**What stays in the database:**

- `target_branch`, `auto_merge`, `sync_enabled`, `sync_remote` — these are per-installation config, not per-codebase
- `setup_commands` and `verification_commands` columns remain as fallback but are deprecated

**Loading priority:**

1. Read `.djinn/settings.json` from the worktree being verified (task branch or target branch)
2. If not present, fall back to project DB config
3. If neither exists, no commands to run (skip verification)

### Part 2: Unified verification service with commit-hash caching

Replace the 5 scattered call sites with a single verification service that caches results by commit hash.

**Core insight:** Same commit hash = same code = same `.djinn/settings.json` = same verification result. Cache passing results; never cache failures (they may be transient — network, disk, flaky test).

**New database table:**

```sql
CREATE TABLE verification_cache (
    project_id  TEXT NOT NULL,
    commit_sha  TEXT NOT NULL,
    passed      INTEGER NOT NULL DEFAULT 1,
    output      TEXT NOT NULL,      -- JSON array of CommandResult
    duration_ms INTEGER NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, commit_sha)
);
```

**Unified function:**

```rust
/// Run setup + verification commands for a given commit.
/// Returns cached result if the same commit was previously verified successfully.
pub async fn verify_commit(
    project_id: &str,
    commit_sha: &str,
    worktree_path: &Path,
    app_state: &AppState,
) -> VerificationResult {
    // 1. Check cache — if this commit already passed, return immediately
    if let Some(cached) = cache.get(project_id, commit_sha) {
        if cached.passed {
            emit_step_event(CacheHit { commit_sha, cached });
            return VerificationResult::Pass(cached);
        }
    }

    // 2. Load commands from .djinn/settings.json in worktree (fall back to DB)
    let commands = load_commands(worktree_path, project_id, app_state).await;

    // 3. Run setup commands, emitting step events
    for cmd in &commands.setup {
        emit_step_event(StepStarted { step: cmd.name });
        let result = run_command(cmd, worktree_path).await;
        emit_step_event(StepFinished { step: cmd.name, result });
        if !result.success { return VerificationResult::Fail(results); }
    }

    // 4. Run verification commands, emitting step events
    for cmd in &commands.verification {
        emit_step_event(StepStarted { step: cmd.name });
        let result = run_command(cmd, worktree_path).await;
        emit_step_event(StepFinished { step: cmd.name, result });
        if !result.success { return VerificationResult::Fail(results); }
    }

    // 5. Cache passing result
    cache.insert(project_id, commit_sha, &results);
    VerificationResult::Pass(results)
}
```

**How each call site changes:**

| Call Site | Before | After |
|-----------|--------|-------|
| Project healthcheck | Creates temp worktree, runs both | `verify_commit(project_id, target_branch HEAD)` — cache hit if main unchanged |
| Worker setup | Runs setup only in task worktree | `verify_commit(project_id, task branch HEAD)` — runs fresh (new commit from worker won't be cached yet). Note: setup still runs in the task worktree so dependencies are installed there. |
| Post-worker verification | Creates fresh worktree, runs both | `verify_commit(project_id, task branch HEAD)` — runs fresh, caches result |
| Pre-merge gate | Creates fresh worktree, runs both | `verify_commit(project_id, task branch HEAD)` — **cache hit** from post-worker verification. Instant. |
| Commands validation | Creates temp worktree, validates new commands | Stays separate — validates proposed commands, no caching |

**Worker setup nuance:** Worker setup serves a dual purpose — it verifies the environment AND installs dependencies in the task worktree. When `verify_commit` gets a cache hit, setup commands already passed for this commit. But the worktree may not have `node_modules` or `target/` populated. Two options:

- **Option A (simple)**: Always run setup commands in the task worktree regardless of cache. Only cache verification results. Setup is typically fast (incremental builds, `npm ci` with lockfile).
- **Option B (optimize later)**: Cache setup separately, skip only if the worktree was recently set up for the same commit.

**Decision: Option A.** Setup commands always run in the worker worktree. Only verification results are cached by commit hash. This keeps the caching logic simple and correct — setup has side effects (populating `node_modules`, `target/`), verification is a pure check.

**Cache eviction:**

- Prune entries older than 7 days on startup
- Clear all entries for a project when `.djinn/settings.json` changes content hash (detected at load time)
- No size limit needed — entries are small (one row per commit per project)

### Part 3: SSE step events for UI observability

Emit granular progress events so the desktop can show a CI/CD-style step log.

**New DjinnEvent variants:**

```rust
/// Progress event for verification/setup command execution.
/// Emitted during project healthcheck and task verification.
DjinnEvent::VerificationStep {
    /// Project ID (always present)
    project_id: String,
    /// Task ID (None for project healthcheck)
    task_id: Option<String>,
    /// Which phase is running
    phase: VerificationPhase,  // Setup | Verification | CacheHit
    /// Step details
    step: StepEvent,
}

enum StepEvent {
    /// A command is about to run
    Started {
        index: u32,
        total: u32,
        name: String,
        command: String,
    },
    /// A command completed
    Finished {
        index: u32,
        name: String,
        exit_code: i32,
        duration_ms: u64,
        stdout: String,  // Full output (truncated to 100 lines)
        stderr: String,
    },
    /// Entire phase completed
    PhaseComplete {
        passed: bool,
        total_duration_ms: u64,
    },
    /// Result was served from cache
    CacheHit {
        commit_sha: String,
        cached_at: String,
        original_duration_ms: u64,
    },
}

enum VerificationPhase {
    Setup,
    Verification,
}
```

**Lifecycle step events (task setup progress):**

```rust
/// Progress event for task lifecycle steps before agent session starts.
DjinnEvent::TaskLifecycleStep {
    task_id: String,
    step: LifecycleStepEvent,
}

enum LifecycleStepEvent {
    WorktreeCreating,
    WorktreeCreated { duration_ms: u64 },
    BranchCreating { name: String },
    BranchCreated { name: String, duration_ms: u64 },
    BranchRebasing { onto: String },
    BranchRebased { onto: String, duration_ms: u64 },
    CredentialLoading { provider: String },
    CredentialLoaded { provider: String },
    SessionCreating,
    SessionCreated { session_id: String },
}
```

### Part 4: Desktop UI — step log component

**Sidebar project indicator enhancement:**

- **Idle**: No dot (current)
- **Checking** (yellow pulse): Healthcheck running. Hover tooltip shows current step name.
- **Healthy + Running** (green pulse): Current behavior.
- **Unhealthy** (red solid): Hover shows error summary. Click opens health log panel.
- **Paused** (yellow solid): Current behavior.

**StepLog component** (new, reusable):

A GitHub Actions-style collapsible step viewer built with shadcn `Accordion` + `ScrollArea`:

```
┌──────────────────────────────────────────────┐
│ ✓ Pre-flight checks                   0.2s   │
│ ✓ cargo build                        12.4s   │
│ ✗ cargo clippy -- -D warnings        45.1s   │ ← red, auto-expanded
│   ┌────────────────────────────────────────┐  │
│   │ $ cargo clippy -- -D warnings          │  │
│   │ error: unused variable `x`             │  │
│   │   --> src/main.rs:42:9                 │  │
│   │ error: could not compile `server`      │  │
│   └────────────────────────────────────────┘  │
│ ○ cargo test                        skipped   │ ← grayed out
└──────────────────────────────────────────────┘
```

Each step row shows: status icon (spinner/check/x/circle) + name + duration. Failed steps auto-expand to show stdout/stderr in a monospace scroll area. A "Cache hit" step shows a lightning bolt icon with the original duration.

**Where it appears:**

1. **Project health flyout**: Click the sidebar dot → slide-over panel with StepLog for the latest healthcheck run.
2. **TaskDetailPanel**: New collapsible "Setup & Verification" section showing lifecycle steps and command results.
3. **TaskSessionPage**: Same section above the session thread.

**New shadcn components to install:** `accordion`, `collapsible`, `scroll-area`.

**Data flow:**

```
Server: verify_commit() / run_task_lifecycle()
  → emit VerificationStep / TaskLifecycleStep events
  → broadcast channel → SSE endpoint
  → Desktop EventSource
  → New Zustand store: useVerificationStore
    - Accumulates steps per (project_id, task_id) pair
    - Clears on new run
  → StepLog component renders from store
```

Steps are ephemeral (in-memory store only). Historical results are available via the existing `commands_run` activity log entries and the new `verification_cache` table.

## Consequences

### Positive

- **Self-updating verification** — tasks that change the build system update verification in the same commit. No manual reconfiguration needed.
- **Shared config** — teammates clone the repo and get the same verification pipeline. No per-developer DB setup.
- **3x faster task lifecycle** — pre-merge gate becomes a cache hit after post-worker verification. Healthcheck skips when main hasn't changed.
- **Full observability** — every command step is visible in real-time with output, timing, and pass/fail status. Developers can see exactly what failed and why.
- **Deterministic caching** — same commit = same result. No stale cache, no manual invalidation (except settings file changes).
- **Healthcheck invalidation solved** — new commits on main = new hash = cache miss = fresh healthcheck.

### Negative

- **File in repo** — `.djinn/settings.json` adds a file to every Djinn-managed repo. Mitigated: the `.djinn/` directory already exists and is git-tracked (ADR-006).
- **Migration period** — during transition, both file and DB config must be supported. The file takes priority; DB is fallback.
- **Cache storage** — one row per project per commit. Minimal size (< 1KB per entry), pruned after 7 days.
- **New SSE events** — adds 2 new DjinnEvent variants. Desktop must handle them (graceful degradation if not — just won't show steps).

### Neutral

- **DB columns remain** — `setup_commands` and `verification_commands` stay in the projects table as fallback. Deprecated but not removed.
- **`project_commands_set`/`get` MCP tools** — kept for backward compatibility and for projects that haven't adopted `.djinn/settings.json` yet. Could add a tool to initialize the file from DB config.

## Migration Strategy

### Phase 1: Unified verification service + caching (server)

1. Add `verification_cache` table migration
2. Create `VerificationService` with `verify_commit()` function
3. Add `load_commands()` that reads `.djinn/settings.json` with DB fallback
4. Refactor all 5 call sites to use `VerificationService`
5. Add commit-hash cache lookup and insertion
6. Worker setup: always runs setup commands in worktree (no cache for setup side-effects)

### Phase 2: SSE step events (server)

1. Add `VerificationStep` and `TaskLifecycleStep` DjinnEvent variants
2. Emit step events from `verify_commit()` and `run_task_lifecycle()`
3. Wire through broadcast channel and SSE endpoint
4. Add desktop SSE handler for new event types

### Phase 3: Desktop UI (desktop)

1. Install shadcn `accordion`, `collapsible`, `scroll-area`
2. Build `StepLog` component
3. Add `useVerificationStore` Zustand store
4. Enhance sidebar project dot with healthcheck states
5. Add health flyout panel
6. Add "Setup & Verification" section to TaskDetailPanel / TaskSessionPage

### Phase 4: Cleanup

1. Add `project_commands_init` MCP tool to generate `.djinn/settings.json` from DB config
2. Deprecation warnings on `project_commands_set`/`get` when `.djinn/settings.json` exists
3. Documentation update

## References

- [[ADR-006]]: Project .djinn Directory — Notes Only, Git-Tracked
- [[ADR-014]]: Project Setup & Verification Commands
- [[ADR-009]]: Simplified Execution
