-- The server-wide launcher quota-authority mode.
--
-- Exactly one component may own a task-run's CPU quota: the launcher's
-- per-invocation cgroup leaf (`leaf-v1`) or Kubernetes in-place Pod resize
-- (`resize-v2`). The vocabulary is `djinn_launcher_protocol::
-- LauncherAuthorityProtocol` — the same two wire forms migration 164 pins for
-- `build_pod_permits.{observed,effective}_launcher_protocol` and migration 166
-- pins for `images.launcher_authority_protocol`. There is no third value and no
-- "both" state: a Pod admitted under a mode it does not declare is refused
-- before shell dispatch rather than run under two quota writers or none.
--
-- WHY THIS IS ITS OWN RELATION AND NOT `admission_handoff`
--
-- `admission_handoff` looks like the obvious home: it is already a singleton
-- authority row with an epoch CAS fence and a `v1_mode`. It is the wrong home
-- for three independent reasons.
--
--   1. It is a DIFFERENT authority. Since PR #2825 the row's `epoch`,
--      `v1_mode` and `cap` are read by `InvocationLeaseAuthorityRepository` as
--      the arming authority and reference cap of the PER-INVOCATION cgroup CPU
--      lease. That answers "is this invocation allowed to lift its own quota".
--      This relation answers "which component is allowed to write quota at
--      all". Conflating them puts two unrelated kill switches behind one lock.
--   2. Its epoch is a shared fence. `set_mode_and_cap` bumps `epoch` on every
--      write, so an operator moving the lease cap would invalidate an
--      in-flight authority-mode CAS, and an authority-mode flip would
--      invalidate an in-flight lease-cap change. Neither operator did anything
--      wrong; the collision is purely an artifact of sharing a row.
--   3. The relation is mid-retirement. `phase`, `v0_mode`,
--      `emergency_ack_epoch` and `invocation_ack_epoch` are dead columns whose
--      DROP is owned by `flc5` (task `0rld`).
--      `InvocationLeaseAuthorityRepository` selects exactly `name, epoch,
--      v1_mode, cap, updated_at` SPECIFICALLY so that DROP is not a breaking
--      change. A column added here would either be dropped out from under this
--      feature or would block `flc5`.
--
-- The singleton shape below is the `build_pod_permit_pools` / `org_ai_policy`
-- convention: one literal key, guarded by a CHECK so a second, silently
-- unread authority row cannot be created by accident.
CREATE TABLE launcher_authority_mode (
    mode_key   VARCHAR(32)  NOT NULL PRIMARY KEY,
    -- The one component permitted to write task-run CPU quota.
    mode       VARCHAR(32)  NOT NULL,
    -- Compare-and-swap fence for operator writes, and nothing more. It is not
    -- an acknowledgement protocol: no reader waits on it and no reader is
    -- disarmed by it.
    epoch      BIGINT       NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    CONSTRAINT launcher_authority_mode_singleton_check
        CHECK (mode_key = 'global'),
    CONSTRAINT launcher_authority_mode_mode_check
        CHECK (mode IN ('leaf-v1', 'resize-v2')),
    CONSTRAINT launcher_authority_mode_epoch_check
        CHECK (epoch >= 0)
);

-- Seeded at `leaf-v1`: the behavior that predates the protocol existing. A
-- deployment that upgrades into this migration keeps writing launcher leaf
-- quota exactly as it does today, and moving to `resize-v2` is an explicit,
-- drain-fenced operator action.
--
-- The seed is deliberately part of the migration rather than a startup
-- fixup. An ABSENT row is not "the default mode"; the repository reports it as
-- `Uninitialized` and every caller fails closed, because a mode nobody wrote
-- must never be read as a mode somebody chose.
INSERT INTO launcher_authority_mode (mode_key, mode) VALUES ('global', 'leaf-v1')
ON CONFLICT (mode_key) DO NOTHING;
