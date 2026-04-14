-- Prototype SQL for replacing SQLite FTS5/BM25 note search with Dolt/MySQL FULLTEXT.
--
-- This file is intentionally NOT a refinery migration because the current runtime
-- still targets SQLite. It documents the schema/query shape needed for ADR-055
-- follow-on implementation once the MySQL/Dolt backend lands.

ALTER TABLE notes
    ADD FULLTEXT INDEX notes_ft (title, content, tags);

-- Ranked note search used by NoteRepository::search lexical candidate generation.
SELECT
    n.id,
    MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE) AS fulltext_score
FROM notes n
WHERE n.project_id = ?
  AND (? = '' OR n.folder = ?)
  AND (? = '' OR n.note_type = ?)
  AND MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE)
ORDER BY fulltext_score DESC, n.id ASC
LIMIT ?;

-- Dedup candidate lookup replacement for NoteRepository::dedup_candidates.
-- Threshold must be recalibrated against MATCH() score distribution.
SELECT
    n.id,
    n.permalink,
    n.title,
    n.folder,
    n.note_type,
    n.abstract,
    n.overview,
    MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE) AS score
FROM notes n
WHERE n.project_id = ?
  AND n.folder = ?
  AND n.note_type = ?
  AND MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE) > ?
ORDER BY score DESC, n.id ASC
LIMIT ?;

-- Contradiction candidate lookup replacement for
-- NoteRepository::detect_contradiction_candidates.
SELECT
    n.id,
    n.permalink,
    n.title,
    n.folder,
    n.note_type,
    MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE) AS score
FROM notes n
WHERE n.id != ?
  AND MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE) > ?
ORDER BY score DESC, n.id ASC
LIMIT 3;

-- Discovery query replacement for build_context -> run_rrf_discovery -> fts_candidates.
SELECT
    n.id,
    MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE) AS fulltext_score
FROM notes n
WHERE n.project_id = ?
  AND MATCH(n.title, n.content, n.tags) AGAINST (? IN BOOLEAN MODE)
ORDER BY fulltext_score DESC, n.id ASC
LIMIT ?;
