-- Immutable catalog material consumed only by strict canonical verification.
-- Nullable preserves ordinary dispatch compatibility while strict resolution
-- rejects legacy catalog rows until their controller-supplied digest is stored.
ALTER TABLE service_presets
    ADD COLUMN IF NOT EXISTS image_digest VARCHAR(71) NULL,
    ADD COLUMN IF NOT EXISTS verification_protocol_revision INT NULL;

ALTER TABLE service_presets
    ADD CONSTRAINT service_preset_image_digest_format CHECK
      (image_digest IS NULL OR image_digest ~ '^sha256:[0-9a-f]{64}$');
ALTER TABLE service_presets
    ADD CONSTRAINT service_preset_protocol_revision_positive CHECK
      (verification_protocol_revision IS NULL OR verification_protocol_revision > 0);

-- Bootstrap entries remain dispatch-compatible but are verification-eligible only
-- with explicit immutable catalog material. Production catalog controllers replace
-- these values with the wrapper image digests they publish.
UPDATE service_presets
SET image_digest = CASE id
    WHEN 'preset-postgres-18' THEN 'sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
    WHEN 'preset-redis-7' THEN 'sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
    WHEN 'preset-rabbitmq-4' THEN 'sha256:2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
    ELSE image_digest
END,
verification_protocol_revision = COALESCE(verification_protocol_revision, 1)
WHERE id IN ('preset-postgres-18', 'preset-redis-7', 'preset-rabbitmq-4');
