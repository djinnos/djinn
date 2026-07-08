-- Active learned-prompt inventory query.
--
-- Source of truth: the runtime active-row predicate in
--   server/crates/djinn-db/src/repositories/agent.rs
-- which derives each agent's live `learned_prompt` from
--   string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
-- over rows with `h.action IN ('keep','confirmed')`.
--
-- This file is the canonical inventory query referenced by the harvest
-- artifact in server/docs/learned-prompt-harvest.md (§3). Run it verbatim
-- against the target environment with a READ-ONLY role. Do not add LIMIT,
-- re-order the result, or edit the predicate — any deviation invalidates
-- the inventory and the downstream harvest gate.
--
-- The `amendment` column alias exposes `lph.proposed_text` (the per-row
-- amendment text) so the export carries exactly what the runtime aggregates.
--
-- Companion helper: server/scripts/learned-prompt-inventory.sh runs this
-- query, writes a TSV export, and prints the row count + SHA-256 checksum
-- evidence that operators paste into §4 of the harvest artifact.
SELECT a.project_id,
       a.id AS agent_id,
       a.name AS agent_name,
       lph.action,
       lph.created_at,
       lph.proposed_text AS amendment
FROM agents a
JOIN learned_prompt_history lph
  ON lph.agent_id = a.id
WHERE lph.action IN ('keep','confirmed')
ORDER BY a.project_id, a.id, lph.created_at ASC;
