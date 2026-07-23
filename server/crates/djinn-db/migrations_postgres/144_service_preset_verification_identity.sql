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
