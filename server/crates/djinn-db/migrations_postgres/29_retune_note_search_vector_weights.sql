-- Retune the `notes.search_vector` tsvector column weighting.
--
-- Migration 1 created the generated tsvector with weights
--   title   = A
--   content = B
--   tags    = C
-- which ranks a content match ABOVE a tag match (ts_rank default weight
-- multipliers are A=1.0, B=0.4, C=0.2, D=0.1). The intended knowledge-base
-- ranking is title > tags > content: a note whose *tag* carries the query
-- term is a stronger signal of relevance than one that merely mentions it in
-- body prose. Re-rank to
--   title   = A   (strongest)
--   tags    = B
--   content = C   (weakest)
--
-- A generated column's expression cannot be altered in place, so we drop the
-- GIN index + generated column and recreate both with the new weighting.
DROP INDEX IF EXISTS notes_search_vector_idx;

ALTER TABLE notes DROP COLUMN IF EXISTS search_vector;

ALTER TABLE notes ADD COLUMN search_vector tsvector
    GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(tags::text, '')), 'B') ||
        setweight(to_tsvector('english', coalesce(content, '')), 'C')
    ) STORED;

CREATE INDEX notes_search_vector_idx ON notes USING GIN(search_vector);
