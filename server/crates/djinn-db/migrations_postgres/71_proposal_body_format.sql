-- Add body_format column to proposals and proposal_revisions.
-- Existing rows default to 'markdown'. Values: 'markdown' | 'mdx'.

ALTER TABLE proposals ADD COLUMN body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';

ALTER TABLE proposal_revisions ADD COLUMN body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';
