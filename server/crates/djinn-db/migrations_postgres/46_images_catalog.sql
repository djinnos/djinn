-- 46_images_catalog.sql
--
-- Registered image catalog: a small, curated set of NAMED images (e.g. "Go",
-- "Rust", "Node") that projects pick from a dropdown — instead of every project
-- building its own bespoke image. Ten Go projects share the one "Go" image,
-- built once.
--
-- Additive + fallback for safety on the live system: `projects.selected_image_id`
-- is nullable; dispatch uses the selected catalog image when set, else falls
-- back to the existing per-project `image_*` columns. The per-project build path
-- stays as the fallback until every project is assigned a catalog image; the
-- cut-over to catalog-only is a later cleanup.
--
-- An image's `config` is a djinn_stack EnvironmentConfig (build fields only —
-- verification already left it in migration 44), so the existing Dockerfile
-- generator + content-hash work unchanged. Identity is the content hash + the
-- immutable registry digest (dispatch resolves the digest, not a mutable tag).

CREATE TABLE IF NOT EXISTS images (
    id                      VARCHAR(36)  NOT NULL PRIMARY KEY,
    name                    VARCHAR(255) NOT NULL,
    description             TEXT         NULL,
    -- A djinn_stack EnvironmentConfig (languages+versions, system_packages,
    -- build env, post_build hooks). Drives the Dockerfile generator.
    config                  JSONB        NOT NULL DEFAULT '{}'::jsonb,
    -- compute_environment_hash(config) — set by the controller; an edit that
    -- changes the hash triggers a rebuild.
    config_hash             VARCHAR(128) NULL,
    -- Registry tag + immutable digest, set on a successful build.
    tag                     VARCHAR(512) NULL,
    registry_digest         VARCHAR(255) NULL,
    -- none | building | ready | failed
    status                  VARCHAR(32)  NOT NULL DEFAULT 'none',
    last_error              TEXT         NULL,
    -- Phase C: backing-service presets tasks using this image may request.
    allowed_service_presets JSONB        NOT NULL DEFAULT '[]'::jsonb,
    created_at              VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    updated_at              VARCHAR(64)  NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    CONSTRAINT uq_images_name UNIQUE (name)
);

-- Build-attempt history (one row per dispatched build) — provenance + a fencing
-- handle so a stale Job result can't clobber a newer attempt.
CREATE TABLE IF NOT EXISTS image_builds (
    id           VARCHAR(36) NOT NULL PRIMARY KEY,
    image_id     VARCHAR(36) NOT NULL,
    attempt      INT         NOT NULL,
    status       VARCHAR(32) NOT NULL,  -- building | succeeded | failed
    job_name     VARCHAR(255) NULL,
    error        TEXT        NULL,
    started_at   VARCHAR(64) NOT NULL DEFAULT to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
    completed_at VARCHAR(64) NULL,
    CONSTRAINT fk_image_builds_image FOREIGN KEY (image_id) REFERENCES images(id) ON DELETE CASCADE,
    CONSTRAINT uq_image_builds_attempt UNIQUE (image_id, attempt)
);

-- Project → selected catalog image. NULL = use the per-project build (fallback).
-- RESTRICT: can't delete an image while a project references it (reassign first).
ALTER TABLE projects
    ADD COLUMN selected_image_id VARCHAR(36) NULL
        REFERENCES images(id) ON DELETE RESTRICT;

CREATE INDEX images_status ON images(status);
CREATE INDEX image_builds_image_id ON image_builds(image_id);
CREATE INDEX projects_selected_image_id ON projects(selected_image_id);
