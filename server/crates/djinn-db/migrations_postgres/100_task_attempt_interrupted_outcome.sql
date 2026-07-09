-- Deploy/reap interruptions are environmental: widen task_attempts.outcome to
-- accept the new terminal `interrupted` value.
--
-- An infrastructure interruption (a coordinator deploy/rollout that kills the
-- worker pod, a startup reap of a run orphaned by that deploy, or a k8s pod
-- eviction during a rollout) that lands while the task is still nonterminal is
-- an ENVIRONMENTAL non-attempt — it must not count as a task failure. The
-- session-exit classifier and the startup orphaned-pending reaper now stamp
-- such attempts `interrupted` (instead of `crashed`) so the dispatch
-- reappearance path can recognize them and skip the failure streak / cooldown
-- escalation, while genuine `crashed` / `timed_out` attempts remain failures.
--
-- Applied idempotently: DROP the pre-existing constraint of the same name
-- before re-ADDing it (migration 94 created it) so re-running is safe.
ALTER TABLE task_attempts
    DROP CONSTRAINT IF EXISTS task_attempts_outcome_check;

ALTER TABLE task_attempts
    ADD CONSTRAINT task_attempts_outcome_check
        CHECK (outcome IN (
            'pending', 'submitted',
            'completed', 'reopened', 'crashed', 'timed_out', 'cancelled',
            'loop_guard_tripped', 'spawn_failed', 'deferred',
            'adopted_pr', 'force_closed', 'handoff', 'interrupted'
        ));
