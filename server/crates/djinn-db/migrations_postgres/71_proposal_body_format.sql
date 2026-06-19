-- dpm9: Add body_format column to proposals and proposal_revisions.
--
-- `body_format` discriminates the body encoding: 'markdown' (legacy default)
-- or 'mdx' (block-aware MDX with structured block tags). When set to 'mdx',
-- the block parser in proposal_blocks.rs is used for validation on
-- create/update.
--
-- Both tables get the column so revision snapshots preserve the format.

ALTER TABLE proposals
    ADD COLUMN IF NOT EXISTS body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';

ALTER TABLE proposal_revisions
    ADD COLUMN IF NOT EXISTS body_format VARCHAR(16) NOT NULL DEFAULT 'markdown';
