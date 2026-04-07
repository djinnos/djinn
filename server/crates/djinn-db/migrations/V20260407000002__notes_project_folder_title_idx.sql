-- Covering index for the hot `NoteRepository::list` / `::catalog` / scoped
-- search queries, which all shape as `WHERE project_id = ?1 ORDER BY folder,
-- title`. Without this index SQLite picks `notes_project_content_hash_idx` for
-- the equality probe and then falls back to `USE TEMP B-TREE FOR ORDER BY`,
-- which is what the `slow statement` warnings in the rotating logs track back
-- to. This composite index lets the planner serve both the filter and the
-- ordering from a single index walk.
CREATE INDEX IF NOT EXISTS notes_project_folder_title_idx
    ON notes(project_id, folder, title);
