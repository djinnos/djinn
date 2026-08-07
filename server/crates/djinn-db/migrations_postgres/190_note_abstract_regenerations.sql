-- Migration 190: per-note L0 abstract regeneration state (proposal u46i, AC8).
--
-- The first cut of the abstract backfill decided staleness by matching the
-- abstract's TEXT against a literal prefix the prompt asks the model to emit
-- (`btrim(abstract) NOT ILIKE 'Applies when%'`). That made convergence depend on
-- model compliance:
--
--   * a note whose regenerated abstract did not open with those exact words
--     stayed stale forever;
--   * every sweep restarts from a NULL keyset cursor, so such a note was
--     re-selected and re-paid-for on every run, without bound;
--   * `is_converged()` required "every note current", so it was structurally
--     unreachable while even one note was phrased differently.
--
-- At a plausible 2-5% non-compliance over ~10.7k notes that is 200-500 notes of
-- permanent repeat LLM spend per sweep. A sweep that cannot converge is not a
-- feature with a caveat.
--
-- This table records what actually happened per note instead of inferring it
-- from prose. Staleness becomes "not regenerated under the current prompt
-- version", and a bounded attempt counter parks notes that keep failing so they
-- are reported rather than retried forever.
--
-- Design invariants:
--   * A SEPARATE table, not new columns on `notes`: `notes` is read through
--     `sqlx::query!` compile-time macros whose offline cache would have to be
--     regenerated (`make sqlx-prepare`, which needs a live Postgres) for a
--     schema change. Additive-table-plus-LEFT-JOIN needs none of that.
--   * `note_id` has NO foreign key to `notes`, matching `note_revision_events`
--     (migration 109) and `note_access_events` (188): the ledger outlives the
--     note it describes.
--   * `prompt_version` is the CONTENT HASH of the L0 prompt
--     (`djinn_provider::prompts::memory_l0_prompt_version`). Editing the prompt
--     changes the hash and so invalidates the corpus automatically — there is
--     no hand-maintained version constant to forget to bump.
--   * `prompt_version` is NULL when a note has been attempted but never
--     successfully regenerated. NULL is therefore "attempted, not current",
--     which is exactly what `IS DISTINCT FROM` treats as stale.
--   * `attempts` counts ENQUEUES, not successes, so the bound holds even if the
--     process dies after enqueue and before generation. It prefers under-retry
--     to unbounded retry.
--   * Timestamps are ISO-8601 UTC strings in the same spelling as `notes` and
--     `retrieval_traces`, so all three join and compare identically.
--
-- Additive only: one new table plus indexes. Nothing existing is read,
-- rewritten, or constrained differently, so this is safe to apply to a live
-- database while older binaries are still serving.

CREATE TABLE IF NOT EXISTS note_abstract_regenerations (
    note_id         VARCHAR(36) NOT NULL PRIMARY KEY,
    project_id      VARCHAR(36) NOT NULL,
    -- Prompt version that produced the note's CURRENT abstract. NULL means the
    -- note has been attempted but never succeeded.
    prompt_version  VARCHAR(64) NULL,
    -- Enqueues so far under `attempt_version`. Reset whenever a regeneration
    -- succeeds, and whenever the prompt version changes: a note that could not
    -- be summarised by one prompt deserves a fresh budget under the next, or a
    -- prompt fix could never rescue the notes it was written to rescue.
    attempts        INTEGER NOT NULL DEFAULT 0,
    -- Prompt version the `attempts` count belongs to.
    attempt_version VARCHAR(64) NULL,
    last_attempt_at VARCHAR(64) NULL,
    regenerated_at  VARCHAR(64) NULL,

    CONSTRAINT fk_note_abstract_regenerations_project
        FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    CONSTRAINT chk_note_abstract_regenerations_attempts_non_negative
        CHECK (attempts >= 0)
);

-- The sweep's selection predicate: "notes in this project not yet current,
-- whose attempt count is still under the bound".
CREATE INDEX IF NOT EXISTS idx_note_abstract_regenerations_project_version_attempts
    ON note_abstract_regenerations (project_id, prompt_version, attempts);
