-- Add merge_commit_sha to tasks for per-task squash-merge traceability.
ALTER TABLE tasks ADD COLUMN merge_commit_sha TEXT;
