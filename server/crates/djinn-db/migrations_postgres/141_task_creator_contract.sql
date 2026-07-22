-- Migration 141 — Task creator contract.
--
-- Backfills every NULL `tasks.created_by_user_id` using deterministic typed
-- precedence and then contracts the column to NOT NULL. The migration requires
-- an explicit designated-operator user supplied via the migration-only session
-- GUC `djinn.migration_designated_operator_user_id` (set by the repository-owned
-- migration runner before applying this file). Even on a database with zero
-- residue the designated operator must validate against `users`; there is no
-- automatic admin or synthetic fallback.
--
-- Precedence (only NULL rows are updated; existing attribution is never
-- overwritten):
--   1. Valid source-task creator (typed audit-selection and remediation-hold
--      relations only — free text is never provenance).
--   2. Valid parent-epic creator.
--   3. Valid linked-proposal build owner, then proposal author.
--   4. The validated designated operator (residue tier).
--
-- Every candidate is accepted only through a `users` join (existence check, not
-- membership). Disabled-but-retained users remain valid. Missing/deleted users
-- and malformed or dangling provenance fall through to a later tier.
--
-- Preflight, backfill, zero-NULL assertion, and SET NOT NULL all execute inside
-- the single SQLx migration transaction. Any failure rolls back the whole
-- migration including its `_sqlx_migrations` row.

-- ── 1. Preflight: validate the designated operator before any data change ─────

DO $$
DECLARE
    raw_setting TEXT;
    trimmed_id  TEXT;
    found_id    VARCHAR(36);
BEGIN
    raw_setting := current_setting('djinn.migration_designated_operator_user_id', true);
    IF raw_setting IS NULL THEN
        RAISE EXCEPTION 'creator_contract_designated_operator_unset';
    END IF;
    trimmed_id := btrim(raw_setting);
    IF trimmed_id = '' THEN
        RAISE EXCEPTION 'creator_contract_designated_operator_unset';
    END IF;
    SELECT id INTO found_id FROM users WHERE id = trimmed_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'creator_contract_designated_operator_invalid:%', trimmed_id;
    END IF;
END $$;

-- ── 2. Deterministic backfill of NULL creators ───────────────────────────────

-- Source-task creators from typed durable relations:
--   (a) Audit selections: audit_selections.audit_task_id ->
--       audit_merged_changes.task_id via audit_selections.merged_change_id.
--   (b) Remediation hold tasks: reverse blockers edge (blocking_task_id =
--       target, task_id = source) only when the target carries the exact typed
--       hold label `human-review-hold` or `planner-park-escalation`.
WITH valid_source AS (
    -- (a) Audit-selection source links.
    SELECT sel.audit_task_id AS target_task_id, mc.task_id AS source_task_id
    FROM audit_selections sel
    JOIN audit_merged_changes mc ON mc.id = sel.merged_change_id
    WHERE sel.audit_task_id IS NOT NULL
      AND mc.task_id IS NOT NULL
    GROUP BY sel.audit_task_id, mc.task_id
    UNION
    -- (b) Remediation-hold reverse blocker edges.
    SELECT b.blocking_task_id AS target_task_id, b.task_id AS source_task_id
    FROM blockers b
    WHERE b.blocking_task_id IS NOT NULL
      AND b.task_id IS NOT NULL
      AND EXISTS (
          SELECT 1 FROM tasks ht
          WHERE ht.id = b.blocking_task_id
            AND (ht.labels ? 'human-review-hold'
              OR ht.labels ? 'planner-park-escalation')
      )
    GROUP BY b.blocking_task_id, b.task_id
),
-- Targets with multiple distinct source links are ambiguous: do not guess.
ambiguous_targets AS (
    SELECT target_task_id
    FROM valid_source
    GROUP BY target_task_id
    HAVING COUNT(DISTINCT source_task_id) > 1
),
source_creator AS (
    SELECT vs.target_task_id, src.created_by_user_id AS creator_id
    FROM valid_source vs
    JOIN tasks src ON src.id = vs.source_task_id
    JOIN users u ON u.id = src.created_by_user_id
    WHERE vs.target_task_id NOT IN (SELECT target_task_id FROM ambiguous_targets)
    GROUP BY vs.target_task_id, src.created_by_user_id
),
epic_creator AS (
    SELECT t.id AS target_task_id, e.created_by_user_id AS creator_id
    FROM tasks t
    JOIN epics e ON e.id = t.epic_id
    JOIN users u ON u.id = e.created_by_user_id
    GROUP BY t.id, e.created_by_user_id
),
proposal_candidate AS (
    SELECT t.id AS target_task_id,
           p.build_owner_user_id AS build_owner_id,
           p.author_user_id      AS author_id
    FROM tasks t
    JOIN epics e ON e.id = t.epic_id
    JOIN proposals p ON p.id = e.proposal_id
),
proposal_resolved AS (
    SELECT pc.target_task_id,
           CASE
               WHEN pc.build_owner_id IS NOT NULL
                    AND EXISTS (SELECT 1 FROM users u WHERE u.id = pc.build_owner_id)
               THEN pc.build_owner_id
               WHEN pc.author_id IS NOT NULL
                    AND EXISTS (SELECT 1 FROM users u WHERE u.id = pc.author_id)
               THEN pc.author_id
               ELSE NULL
           END AS creator_id
    FROM proposal_candidate pc
),
resolved_creators AS (
    SELECT t_base.id AS task_id,
           COALESCE(
               sc.creator_id,
               ec.creator_id,
               pr.creator_id,
               btrim(current_setting('djinn.migration_designated_operator_user_id', true))
           ) AS creator
    FROM tasks t_base
    LEFT JOIN source_creator  sc ON sc.target_task_id = t_base.id
    LEFT JOIN epic_creator    ec ON ec.target_task_id = t_base.id
    LEFT JOIN proposal_resolved pr ON pr.target_task_id = t_base.id
)
UPDATE tasks t
SET created_by_user_id = rc.creator
FROM resolved_creators rc
WHERE t.id = rc.task_id
  AND t.created_by_user_id IS NULL;

-- ── 3. Zero-NULL assertion ───────────────────────────────────────────────────

DO $$
DECLARE
    residue_count BIGINT;
BEGIN
    SELECT COUNT(*) INTO residue_count FROM tasks WHERE created_by_user_id IS NULL;
    IF residue_count > 0 THEN
        RAISE EXCEPTION 'creator_contract_null_residue:%', residue_count;
    END IF;
END $$;

-- ── 4. Contract the column to NOT NULL using lowest-lock strategy ────────────

ALTER TABLE tasks
    ADD CONSTRAINT tasks_created_by_user_id_not_null_check
    CHECK (created_by_user_id IS NOT NULL) NOT VALID;

ALTER TABLE tasks
    VALIDATE CONSTRAINT tasks_created_by_user_id_not_null_check;

ALTER TABLE tasks
    ALTER COLUMN created_by_user_id SET NOT NULL;

ALTER TABLE tasks
    DROP CONSTRAINT tasks_created_by_user_id_not_null_check;
