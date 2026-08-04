-- The former generic registry accepted caller-adjacent identities and health.
-- It was never authoritative, so it cannot remain a table that appears to
-- certify typed evidence. Family-specific sources resolve in the repository.
DROP TABLE IF EXISTS typed_evidence_anchor_sources;
DROP FUNCTION IF EXISTS reject_typed_evidence_anchor_source_mutation();
