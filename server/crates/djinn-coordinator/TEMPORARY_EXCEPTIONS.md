# Temporary Exceptions in djinn-coordinator

## Direct `sqlx` usage (to be removed in final boundary task)

The following production files contained direct `sqlx` usage that has been
migrated to `djinn-db` repository/helper methods:

- ~~`src/health.rs`~~ — orphan session reaping, output-stash GC session
  loading, protected cargo-target run-id collection, and session listing
  for task-runs now use `djinn-db` repository methods
  (`SessionRepository::orphan_session_candidates`,
  `SessionRepository::interrupt_by_id`,
  `SessionRepository::list_all_status_ended_at`,
  `SessionRepository::running_task_run_ids`,
  `SessionRepository::list_for_task_run`,
  `TaskRunRepository::running_ids`).

## Remaining test-only `sqlx` usage (acceptable)

- `src/reentrance.rs` — line 455: `sqlx::query("DROP TABLE IF EXISTS
  epic_blockers")` in a `#[cfg(test)]` failure-injection test
  (`blocker_lookup_error_defers_dispatch_fail_closed`).  This is
  **test-only** and deliberately injects a table-missing error to verify
  the fail-closed blocker-lookup behaviour.  The final boundary task
  (`i5mt`) will either move this to `djinn-db` test support or document
  it as a permanent test-fixture exception.

Test files with direct sqlx (acceptable):
- `src/tests/session_reaping.rs`
- `src/tests/doctor_zombie_e2e.rs`
- `src/tests/pause_is_not_fault.rs`

The `djinn-coordinator` `sqlx` dev-dependency will be fully removed
when the remaining test-only SQL fixtures are addressed in the final
boundary enforcement task.
