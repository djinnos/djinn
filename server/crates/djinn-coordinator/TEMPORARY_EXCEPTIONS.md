# Temporary Exceptions in djinn-coordinator

**No exceptions remain.**

All direct `sqlx` usage — both production and test-only — has been migrated
to `djinn-db` repository methods and test-support helpers:

- Production health/reentrance SQL → `djinn-db` repository methods
  (`SessionRepository`, `TaskRepository`, `TaskRunRepository`).
- Test fixture SQL (task status updates, session backdating, token
  counts, task-run row seeding, failure-injection table drops) →
  `djinn-db` test-support helpers (`drop_table_for_test`,
  `backdate_task_updated_at`) and repository methods
  (`SessionRepository::backdate_started_at`,
  `SessionRepository::set_token_counts`,
  `SessionRepository::set_tokens_and_backdate`,
  `TaskRepository::set_status`, `TaskRepository::set_pr_url`,
  `TaskRunRepository::create`).

The `djinn-coordinator` crate no longer depends on `sqlx` directly.
