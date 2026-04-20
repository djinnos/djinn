-- Migration 8: Per-project devcontainer image columns for Phase 3 PR 5.
--
-- Populated by `djinn_image_controller::ImageController`. The controller
-- hashes the committed `.devcontainer/devcontainer.json` +
-- `devcontainer-lock.json` on every mirror-fetch tick; when the hash
-- changes, a build Job is enqueued and `image_status` flips to
-- `'building'`. On success the Job writes back the pushed `image_tag`
-- and `image_hash`; on failure `image_last_error` carries the error.
--
-- Consumers: `KubernetesRuntime::prepare` reads `image_tag` + `image_status`
-- to decide whether to dispatch a task-run Job; the UI devcontainer banner
-- (Phase 3 PR 6) surfaces status + errors.

ALTER TABLE projects
    ADD COLUMN image_tag VARCHAR(512) NULL,
    ADD COLUMN image_hash VARCHAR(128) NULL,
    ADD COLUMN image_status VARCHAR(32) NOT NULL DEFAULT 'none',
    ADD COLUMN image_last_error LONGTEXT NULL;
