# Temporary Exceptions in djinn-coordinator

## Direct `sqlx` usage (to be removed in later Wave 2 tasks)

The following production files contain direct `sqlx` usage that should be
migrated to `djinn-db` repository methods:

- `src/health.rs` — orphan session reaping queries (`sqlx::query_as`,
  `sqlx::query`, `sqlx::query_scalar`) against the `sessions` and
  `task_runs` tables
- `src/reentrance.rs` — dispatch-state queries

Test files with direct sqlx (acceptable):
- `src/tests/session_reaping.rs`
- `src/tests/doctor_zombie_e2e.rs`
- `src/tests/pause_is_not_fault.rs`

These will be eliminated when the corresponding query surfaces are lifted
into `djinn-db` proper (tracked in Wave 2 Task 3: "Thin djinn-agent
coordinator/doctor/supervisor modules to re-export facade").
