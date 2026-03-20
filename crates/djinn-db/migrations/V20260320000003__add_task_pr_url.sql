-- ADR-a8le: Add pr_url column to tasks table.
-- Stores the GitHub PR URL created by the reviewer when GitHub App is connected.
-- NULL when the direct-push merge path is used (no GitHub App).
ALTER TABLE tasks ADD COLUMN pr_url TEXT;
