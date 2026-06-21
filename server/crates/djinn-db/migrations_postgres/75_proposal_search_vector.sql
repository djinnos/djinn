-- poeg: add FTS search_vector on proposals for unified search surface.
--
-- Adds a generated tsvector column `search_vector` on the `proposals` table
-- with weighted fields mirroring the notes convention (migration 29):
--   title              = A  (strongest)
--   body               = B
--   acceptance_criteria = C  (weakest, cast to text for tsvector)
--
-- A GIN index enables efficient `@@` lookups so ProposalRepository::search_proposals
-- can rank with ts_rank() and highlight with ts_headline().

ALTER TABLE proposals ADD COLUMN IF NOT EXISTS search_vector tsvector
    GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(body, '')), 'B') ||
        setweight(to_tsvector('english', coalesce(acceptance_criteria::text, '')), 'C')
    ) STORED;

CREATE INDEX IF NOT EXISTS proposals_search_vector_idx
    ON proposals USING GIN (search_vector);
