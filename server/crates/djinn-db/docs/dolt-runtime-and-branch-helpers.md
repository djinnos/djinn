# ADR-055 Dolt SQL branch helper integration coverage

This repository now includes executable seams for Dolt runtime management and branch lifecycle SQL:

- `server/src/db/dolt.rs` manages `dolt sql-server` startup/health probing.
- `server/src/db/runtime.rs` calls the Dolt runtime seam before opening a Dolt-backed MySQL pool.
- `server/crates/djinn-db/src/repositories/dolt_branch.rs` provides helper operations for `DOLT_BRANCH`, `DOLT_CHECKOUT`, `DOLT_MERGE`, and branch delete.
- `server/crates/djinn-db/src/repositories/dolt_history_maintenance.rs` plans ADR-055 compaction/flatten maintenance windows, captures baseline row counts, and blocks destructive maintenance when verification or branch-safety guards fail.

## Covered scenarios

1. **Unavailable runtime**
   - `server/src/db/dolt.rs::tests::unavailable_runtime_surfaces_actionable_error`
   - Verifies a Dolt backend without a healthy server or managed repo config returns an actionable error.

2. **Healthy startup via managed sql-server**
   - `server/src/db/dolt.rs::tests::manager_can_spawn_and_probe_fake_sql_server`
   - Verifies the manager can spawn a fake `dolt sql-server` replacement and observe health.

3. **At least one branch lifecycle action**
   - `server/crates/djinn-db/src/repositories/dolt_branch.rs::tests`
   - Verifies helper contract behavior for task branch naming and backend gating for lifecycle operations.

4. **Lifecycle maintenance planning + safety checks**
   - `server/crates/djinn-db/src/repositories/dolt_history_maintenance.rs::tests`
   - Verifies compact-vs-flatten scheduling, blocks maintenance when task branches are present, and aborts on row-count verification mismatch.

The branch helper is intentionally thin: it centralizes the Dolt stored procedure contract so coordinator/session/promotion flows can reuse one SQL seam instead of issuing ad hoc `CALL DOLT_*` statements.
