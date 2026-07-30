-- A catalog image may DECLARE which component owns the CPU quota of the
-- invocation leaf its `djinn-cgroup-launcher` sidecar creates. The vocabulary
-- is `djinn_launcher_protocol::LauncherAuthorityProtocol` — the same two wire
-- forms migration 164 pins for `build_pod_permits.observed_launcher_protocol` /
-- `effective_launcher_protocol`.
--
-- The column is deliberately NULLABLE. Every image built before this migration
-- carries no declaration, and those rows must keep dispatching exactly as they
-- do today under the pre-existing `leaf-v1` behavior. NULL means "legacy build,
-- no handshake" — never "unknown protocol". Making the digest requirement below
-- unconditional would strand every already-built image on a live deployment.
ALTER TABLE images ADD COLUMN launcher_authority_protocol VARCHAR(32);

ALTER TABLE images
    ADD CONSTRAINT images_launcher_authority_protocol_check CHECK (
        launcher_authority_protocol IS NULL
        OR launcher_authority_protocol IN ('leaf-v1', 'resize-v2')
    ),
    -- Migration 164's `build_pod_permits_resize_identity_check` requires
    -- `image_digest IS NOT NULL` whenever resize identity is present. An image
    -- that declares a protocol but carries no immutable manifest digest
    -- therefore yields a build Pod that can never capture that identity:
    -- bootstrap fails closed and the task run cannot dispatch, with nothing
    -- surfacing the gap until dispatch stops.
    --
    -- Refuse the row instead of the Pod. Scoped to protocol-DECLARING rows
    -- only: a pre-existing `status = 'ready'` row with a NULL digest also has a
    -- NULL protocol and is untouched by this constraint.
    ADD CONSTRAINT images_declared_protocol_requires_digest_check CHECK (
        launcher_authority_protocol IS NULL
        OR registry_digest IS NOT NULL
    );
