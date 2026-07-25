-- ij6g: catalog wrapper artifact identity.
--
-- The deployable catalog service image is the *wrapper* artifact, which packages
-- the stock service runtime plus the protocol-v1 control server in one image.
-- The wrapper is identified by `{wrapper_image}@{image_digest}`: `wrapper_image`
-- is a mutable repository reference and `image_digest` is the immutable manifest
-- digest that strict canonical verification pins.
--
-- Migration 147 seeded fabricated placeholder digests directly into
-- `image_digest`. Those literals are not deployable images. This migration
-- removes them so strict resolution fails closed until the build/controller path
-- records a real published digest, and records the catalog-owned wrapper
-- repositories the build produces.
ALTER TABLE service_presets
    ADD COLUMN IF NOT EXISTS wrapper_image TEXT NULL;

-- A wrapper repository reference is a plain repository ref (optionally tagged)
-- with no digest suffix; the immutable digest lives only in `image_digest`.
ALTER TABLE service_presets
    ADD CONSTRAINT service_preset_wrapper_image_no_digest CHECK
      (wrapper_image IS NULL OR wrapper_image NOT LIKE '%@%');

-- Record the catalog-owned wrapper repositories and clear the fabricated
-- placeholder digests seeded by migration 147. The real immutable digests are
-- published by the build/controller path (see djinn-image-controller
-- wrapper_catalog reconciliation); until then `image_digest` stays NULL and
-- strict resolution rejects the preset.
UPDATE service_presets
SET wrapper_image = CASE id
    WHEN 'preset-postgres-18' THEN 'ghcr.io/djinnos/djinn-postgres-wrapper'
    WHEN 'preset-redis-7' THEN 'ghcr.io/djinnos/djinn-redis-wrapper'
    WHEN 'preset-rabbitmq-4' THEN 'ghcr.io/djinnos/djinn-rabbitmq-wrapper'
    ELSE wrapper_image
END,
image_digest = NULL
WHERE id IN ('preset-postgres-18', 'preset-redis-7', 'preset-rabbitmq-4');
