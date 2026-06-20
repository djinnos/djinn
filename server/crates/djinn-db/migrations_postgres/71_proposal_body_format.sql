-- na13: Add body_format column to proposals and proposal_revisions.
--
-- `body_format` distinguishes legacy `markdown` (plain spec) from `mdx`
-- (block-aware). The column is NOT NULL with a DEFAULT of `'markdown'` so
-- every existing row inherits the legacy format without a data backfill.
--
-- Both tables get the column so revision snapshots preserve the format.

ALTER TABLE proposals
    ADD COLUMN IF NOT EXISTS body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';

ALTER TABLE proposal_revisions
    ADD COLUMN IF NOT EXISTS body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';
