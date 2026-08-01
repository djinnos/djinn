-- Fresh installations use the post-cutover Kubernetes resize authority.
--
-- Migration 167 is immutable once applied.  Its leaf-v1 seed is moved forward
-- here only while the singleton remains untouched at epoch zero.  Any operator
-- CAS (including an explicit selection of leaf-v1) advances the epoch and is
-- therefore preserved byte-for-byte.
UPDATE launcher_authority_mode
SET mode = 'resize-v2',
    updated_at = now()
WHERE mode_key = 'global'
  AND epoch = 0;
