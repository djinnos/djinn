-- Extend task issue_type to support spike, research, decomposition, and review.
-- SQLite TEXT has no enum constraint at the DB layer; validation is enforced by
-- the application (djinn-mcp validate_issue_type). This migration is a schema
-- documentation marker so the migration version is recorded in refinery's history.
-- No structural change is needed: the column already exists as free-form TEXT.
SELECT 1; -- no-op statement to satisfy refinery's migration runner
