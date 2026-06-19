-- Add body_format to proposals and proposal_revisions.
-- 'markdown' (default, legacy) or 'mdx' (block-aware).
ALTER TABLE proposals ADD COLUMN body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';
ALTER TABLE proposal_revisions ADD COLUMN body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';
