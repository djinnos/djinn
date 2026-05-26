-- Fix MySQL→Postgres cut-over typing for SHA-256/hash columns.
--
-- These were created as fixed-width CHAR(64), which under Postgres `bpchar`
-- semantics pads values with trailing spaces on read. Full 64-char hex hashes
-- are unaffected, but any shorter value reads back space-padded — corrupting
-- equality comparisons — and the typing is inconsistent with `code_chunks`,
-- which already uses VARCHAR(64). Convert to VARCHAR(64), trimming any padding
-- already persisted. `event_key` is a PRIMARY KEY; ALTER TYPE transparently
-- rebuilds its index. (repo_map_cache, also CHAR(64) in migration 1, was
-- dropped in migration 14, so it is intentionally not touched here.)
ALTER TABLE notes
    ALTER COLUMN content_hash TYPE VARCHAR(64) USING rtrim(content_hash);

ALTER TABLE note_embedding_meta
    ALTER COLUMN content_hash TYPE VARCHAR(64) USING rtrim(content_hash);

ALTER TABLE coupling_pair_events
    ALTER COLUMN event_key TYPE VARCHAR(64) USING rtrim(event_key);
