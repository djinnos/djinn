-- Persist the wire-format discriminator with each immutable galaxy artifact.
-- Defaults preserve the existing publication API while allowing readers to
-- identify rollout formats before any response headers are formed.
ALTER TABLE repo_graph_galaxy_artifact
    ADD COLUMN IF NOT EXISTS artifact_version INTEGER NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS encoding VARCHAR(32) NOT NULL DEFAULT 'gzip',
    ADD CONSTRAINT repo_graph_galaxy_artifact_version_positive
        CHECK (artifact_version > 0),
    ADD CONSTRAINT repo_graph_galaxy_artifact_encoding_header_safe
        CHECK (encoding ~ '^[!-~]+$');
