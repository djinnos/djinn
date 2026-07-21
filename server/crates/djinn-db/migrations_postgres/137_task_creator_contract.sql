-- Task creator contract. This migration is deliberately transactional: sqlx
-- executes normal migration files in one transaction and this file must never
-- gain the nontransactional migration directive.
--
-- Only durable, typed relations participate in provenance. In particular,
-- task prose, activity/comments and ordinary blocker edges are not evidence.
DO $$
DECLARE
    designated_operator_id TEXT;
BEGIN
    designated_operator_id := NULLIF(btrim(current_setting('djinn.migration_designated_operator_user_id', true)), '');
    IF designated_operator_id IS NULL THEN
        RAISE EXCEPTION 'creator_contract_designated_operator_unset';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM users WHERE id = designated_operator_id) THEN
        RAISE EXCEPTION 'creator_contract_designated_operator_invalid:%', designated_operator_id;
    END IF;
END $$;

-- A source is usable only when its typed evidence produces exactly one extant
-- user. The audit path is audit_task -> selection -> merged change -> source
-- task. A remediation hold is the narrow persisted shape where a review task
-- blocks a source task; generic blocker edges are intentionally excluded.
WITH source_candidates AS (
    SELECT audit_task.id AS task_id, source_task.created_by_user_id AS user_id
    FROM tasks AS audit_task
    JOIN audit_selections AS selection ON selection.audit_task_id = audit_task.id
    JOIN audit_merged_changes AS merged_change ON merged_change.id = selection.merged_change_id
    JOIN tasks AS source_task ON source_task.id = merged_change.task_id
    JOIN users AS source_user ON source_user.id = source_task.created_by_user_id
    WHERE audit_task.created_by_user_id IS NULL

    UNION ALL

    SELECT hold_task.id AS task_id, source_task.created_by_user_id AS user_id
    FROM tasks AS hold_task
    JOIN blockers AS hold ON hold.blocking_task_id = hold_task.id
    JOIN tasks AS source_task ON source_task.id = hold.task_id
    JOIN users AS source_user ON source_user.id = source_task.created_by_user_id
    WHERE hold_task.created_by_user_id IS NULL
      AND hold_task.issue_type = 'review'
      AND hold_task.owner = 'system'
      AND hold_task.labels ?| ARRAY['human-review-hold', 'planner-park-escalation']
),
unique_sources AS (
    SELECT task_id, min(user_id) AS user_id
    FROM source_candidates
    GROUP BY task_id
    -- Two source relations are ambiguous even when they name the same user.
    HAVING count(*) = 1
),
resolved AS (
    SELECT task.id,
           COALESCE(
               source.user_id,
               epic_user.id,
               build_owner.id,
               proposal_author.id,
               (SELECT id FROM users WHERE id = NULLIF(btrim(current_setting('djinn.migration_designated_operator_user_id', true)), ''))
           ) AS user_id
    FROM tasks AS task
    LEFT JOIN unique_sources AS source ON source.task_id = task.id
    LEFT JOIN epics AS epic ON epic.id = task.epic_id
    LEFT JOIN users AS epic_user ON epic_user.id = epic.created_by_user_id
    LEFT JOIN proposals AS proposal ON proposal.id = epic.proposal_id
    LEFT JOIN users AS build_owner ON build_owner.id = proposal.build_owner_user_id
    LEFT JOIN users AS proposal_author ON proposal_author.id = proposal.author_user_id
    WHERE task.created_by_user_id IS NULL
)
UPDATE tasks AS task
SET created_by_user_id = resolved.user_id
FROM resolved
WHERE task.id = resolved.id;

DO $$
DECLARE
    null_residue BIGINT;
BEGIN
    SELECT count(*) INTO null_residue FROM tasks WHERE created_by_user_id IS NULL;
    IF null_residue <> 0 THEN
        RAISE EXCEPTION 'creator_contract_null_residue:%', null_residue;
    END IF;
END $$;

ALTER TABLE tasks
    ADD CONSTRAINT tasks_created_by_user_id_not_null
    CHECK (created_by_user_id IS NOT NULL) NOT VALID;
ALTER TABLE tasks VALIDATE CONSTRAINT tasks_created_by_user_id_not_null;
ALTER TABLE tasks ALTER COLUMN created_by_user_id SET NOT NULL;
ALTER TABLE tasks DROP CONSTRAINT tasks_created_by_user_id_not_null;
