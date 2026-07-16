# Raw-Signal Bypass Audit Runbook

> **Epic:** End-to-end liveness regression hardening and raw-signal bypass audit (ptvg)
> **Proposal:** twis — Reaper liveness discrimination: crash-vs-slow three-layer defense + stranded-task detection
> **Guard scope:** Every in-scope consumer that decides session/task fate must consult
>   the shared liveness classifier (`djinn_coordinator::dispatch::liveness::classify`)
>   or its persisted verdict (`LivenessRepository`) rather than independently
>   deciding from raw pod phase, in-memory activity, or DB session/task state alone.

## Background

The liveness classifier (`classify()`) is the single source of truth for session
liveness decisions. It combines normalized evidence (pod phase, activity signal,
DB session/task status, claim TTL, extension budget, hard runtime deadline, exit
code) into a structured `ClassificationResult` carrying a `Verdict`, `Outcome`,
and `Reason`. Consumers must use this classifier — or consult its persisted
output — rather than re-implementing their own heuristics from the raw input
signals.

Bypassing the classifier risks:
- **False reap:** a productive session killed because a raw heuristic misread
  the evidence (e.g. zero-token running session where tokens are flushed only
  at session end).
- **False miss:** a genuinely dead session spared because the raw check
  evaluated stale in-memory state while the DB truth diverged.
- **Inconsistent verdicts:** different consumers reaching different conclusions
  from the same evidence, making the diagnostics surface unreliable.

## In-Scope Consumers

Each consumer below must integrate with the classifier or its stored verdict.
The "evidence of integration" column lists the expected code patterns that a
grep-guard or manual audit should verify.

| # | Consumer | Module / Entry Point | Evidence of Integration |
|---|----------|---------------------|------------------------|
| 1 | **Stall recovery** | `session_recovery::enforce_session_stall_timeout` | Calls `classify_task_liveness()`, persists `LivenessEvidenceSnapshot` with `verdict: "dead"` and `outcome_kind: "dead_reclaimed"` before terminal transition |
| 2 | **Zombie-running recovery** | `session_recovery::reap_zombie_sessions` | Calls `classify_task_liveness()` and gates reap on `Verdict::Dead`; `Verdict::Live`/`Verdict::Slow` suppresses reap |
| 3 | **Idle/session settle** | `session_recovery::reap_idle_chat_sessions` | Persists `LivenessEvidenceSnapshot` with `verdict: "dead"` and `outcome_kind: "dead_reclaimed"` |
| 4 | **Explicit kill-task teardown** | `session_recovery` kill paths (ceiling, stall) | Each kill path persists `LivenessEvidenceSnapshot` with appropriate verdict/outcome before terminal transition |
| 5 | **Respawn/retry** | `dispatch/retry.rs`, `dispatch/respawn_guard.rs` | Retry accounting uses `TaskAttemptOutcome` derived from classifier verdict; protocol violations increment attempts |
| 6 | **DB board_health** | `djinn_db::TaskRepository::board_health` | Surfaces `liveness_outcomes.recent[].verdict`, `outcome_kind`, `outcome_reason` from `liveness_evidence` table |
| 7 | **Control-plane board_health** | `djinn-control-plane` MCP `board_health` tool | Surfaces DB liveness_outcomes/protocol_violations/stranded_ready sections; `stranded_ready.findings[].dispatch_gate` carries gate evidence |

## Audit Procedure

### Automated (grep-guard style)

Run the companion test `raw_signal_bypass_grep_guard` in the coordinator test
suite. It verifies that `session_recovery.rs` contains:
- `classify_task_liveness` call sites
- `ClassificationResult` usage
- `LivenessEvidenceSnapshot` persistence
- `LivenessOutcome::KillNoop` terminal-task guard
- `Verdict::Live` / `Verdict::Slow` suppression arms

If the test fails after a refactor, the refactored code has dropped classifier
integration and must be restored before merge.

### Manual Checklist

When touching any in-scope consumer, verify:

- [ ] The consumer does **not** directly inspect `pod_phase`, `activity`, or
      `db_session_status` to decide whether to kill/reap/retry.
- [ ] The consumer calls `classify()` or `classify_task_liveness()`, or reads
      the persisted verdict from `LivenessRepository`.
- [ ] A `Live` or `Slow` verdict from the classifier suppresses destructive
      action (kill, reap, reopen).
- [ ] A terminal-task `KillNoop` outcome prevents reopening or retrying an
      already-finished task.
- [ ] Evidence is persisted as a `LivenessEvidenceSnapshot` before any
      terminal transition (kill, reap, finalize).
- [ ] The `board_health` MCP surface and the coordinator doctor tick surface
      the same verdict, outcome, and evidence IDs for a shared session.

### Non-Automatable Proofs

The following verifications cannot be fully automated in a unit/integration
test and are tracked here as operator obligations:

1. **Doctor tick cadence:** Verify in production logs that the cheap-doctor
   subset runs at least once per 30s coordinator tick cycle. The
   `djinn_doctor_run_duration_seconds` metric confirms this.
2. **MCP surface shape stability:** After a schema migration that adds columns
   to `liveness_evidence`, verify that `board_health` gracefully returns
   defaults (not errors) for the new columns. The MCP contract test proves
   the current shape; new columns may need additional assertions.
3. **Operator dashboard parity:** If a monitoring dashboard independently
   queries `liveness_evidence`, verify it reads the same verdict/outcome
   columns as `board_health` to avoid displaying stale or contradictory
   information.

## References

- `server/crates/djinn-coordinator/src/dispatch/liveness.rs` — Pure classifier
- `server/crates/djinn-coordinator/src/dispatch/session_recovery.rs` — Stall/zombie/idle consumers
- `server/crates/djinn-coordinator/src/dispatch/retry.rs` — Retry accounting
- `server/crates/djinn-coordinator/src/doctor/leader_tick.rs` — Doctor leader-tick integration
- `server/crates/djinn-db/src/repositories/task/board_health.rs` — DB board_health sections
- `server/crates/djinn-control-plane/tests/board_tools.rs` — MCP contract tests
- `server/crates/djinn-db/tests/task_tests/state_machine.rs` — DB state-machine tests
- `server/crates/djinn-db/tests/task_tests/board_health.rs` — DB board_health compatibility and liveness/stranded-ready tests
