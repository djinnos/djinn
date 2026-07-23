-- Bind final verification to the image selected when this task run was dispatched.
-- Legacy rows remain NULL and are deliberately ineligible for strict verification.
ALTER TABLE task_runs ADD COLUMN IF NOT EXISTS catalog_image_id VARCHAR(36) NULL;
CREATE INDEX IF NOT EXISTS idx_task_runs_catalog_image ON task_runs(catalog_image_id)
    WHERE catalog_image_id IS NOT NULL;
