-- Migration 198 (proposal t5rn, T4): mark and retire legacy pipeline working
-- specifications.
--
-- Every statement is structural or predicate-driven: nothing references a
-- particular project, session, note, or task, and every statement is correct on
-- an empty database (it simply matches nothing). sqlx applies each migration
-- exactly once.
--
-- # What this is fixing
--
-- `persist_working_spec` wrote pipeline scratch as an ordinary active `design`
-- note. `design` is a hand-authored class excluded from automated lifecycle
-- policy, so those rows accumulate forever and are indistinguishable from human
-- design notes by type alone. From this migration forward the pipeline marks
-- them with the reserved `working-spec` tag; this migration retro-marks the rows
-- already in the corpus.
--
-- # The safety posture: false negatives over false positives
--
-- Touching a human-authored design note is the unacceptable outcome, so a row is
-- retro-marked only when EVERY machine predicate holds. Anything that merely
-- looks like a working spec but fails a predicate is left completely unchanged
-- and recorded in `working_spec_migration_report` for manual classification.

-- ── 1. Manual-classification report ──────────────────────────────────────────
--
-- Report-only. Nothing reads this to make a decision; it exists so an operator
-- can review what the migration deliberately declined to touch.
CREATE TABLE IF NOT EXISTS working_spec_migration_report (
    note_id            VARCHAR(36)  NOT NULL PRIMARY KEY,
    project_id         VARCHAR(36)  NOT NULL,
    permalink          VARCHAR(255) NOT NULL,
    title              VARCHAR(512) NOT NULL,
    -- Which machine predicates the row failed, so classification is auditable.
    matched_task_id    VARCHAR(36)  NULL,
    has_canonical_permalink  BOOLEAN NOT NULL,
    has_extraction_revision  BOOLEAN NOT NULL,
    has_constraint_sentence  BOOLEAN NOT NULL,
    recorded_at        VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
);

-- ── 2. Classify every active design note that claims a working-spec title ────
--
-- The title shape `Working Spec {task.short_id}` is only the *entry* condition;
-- it decides nothing on its own. `short_id` is restricted to alphanumerics so
-- the canonical permalink can be reproduced exactly here: `slugify` lowercases
-- and replaces every non-alphanumeric run with a single dash, which for
-- `Working Spec {alnum}` is exactly `working-spec-{lower(short_id)}` under the
-- `design` folder. A short_id outside that character class simply fails to
-- match and is reported — a false negative, which is the safe direction.
CREATE TEMPORARY TABLE t5rn_working_spec_candidates AS
SELECT
    n.id                AS note_id,
    n.project_id        AS project_id,
    n.permalink         AS permalink,
    n.title             AS title,
    t.id                AS matched_task_id,
    (n.permalink = 'design/working-spec-' || lower(t.short_id))
                        AS has_canonical_permalink,
    EXISTS (
        SELECT 1
        FROM note_revision_events r
        WHERE r.note_id = n.id
          AND r.note_seq = (
                SELECT max(r2.note_seq) FROM note_revision_events r2 WHERE r2.note_id = n.id
              )
          AND r.actor_kind = 'system'
          AND r.subsystem = 'extraction'
          AND r.task_id = t.id
    )                   AS has_extraction_revision,
    (n.content LIKE '%This note is task-scoped working context routed from non-durable extraction output.%')
                        AS has_constraint_sentence,
    (t.status = 'closed') AS task_is_terminal
FROM notes n
JOIN tasks t
  ON t.project_id = n.project_id
 AND t.short_id ~ '^[A-Za-z0-9]+$'
 AND n.title = 'Working Spec ' || t.short_id
WHERE n.storage = 'db'
  AND n.status = 'active'
  AND n.note_type = 'design';

-- ── 3. Retro-mark the rows satisfying EVERY predicate ────────────────────────
--
-- The tag is added idempotently and no other tag is disturbed.
UPDATE notes n
SET tags = CASE
        WHEN n.tags @> '["working-spec"]'::jsonb THEN n.tags
        ELSE n.tags || '["working-spec"]'::jsonb
    END
FROM t5rn_working_spec_candidates c
WHERE n.id = c.note_id
  AND c.has_canonical_permalink
  AND c.has_extraction_revision
  AND c.has_constraint_sentence;

-- ── 4. Archive only those whose matched task is already terminal ─────────────
--
-- A marked working spec whose task is still live stays active; the terminal
-- transition will archive it when the task actually closes.
UPDATE notes n
SET status = 'archived',
    lifecycle_changed_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
FROM t5rn_working_spec_candidates c
WHERE n.id = c.note_id
  AND n.status = 'active'
  AND c.has_canonical_permalink
  AND c.has_extraction_revision
  AND c.has_constraint_sentence
  AND c.task_is_terminal;

-- ── 5. Report every near-miss for manual classification ──────────────────────
INSERT INTO working_spec_migration_report (
    note_id, project_id, permalink, title, matched_task_id,
    has_canonical_permalink, has_extraction_revision, has_constraint_sentence
)
SELECT c.note_id, c.project_id, c.permalink, c.title, c.matched_task_id,
       c.has_canonical_permalink, c.has_extraction_revision, c.has_constraint_sentence
FROM t5rn_working_spec_candidates c
WHERE NOT (c.has_canonical_permalink
           AND c.has_extraction_revision
           AND c.has_constraint_sentence)
ON CONFLICT (note_id) DO NOTHING;

DROP TABLE t5rn_working_spec_candidates;
