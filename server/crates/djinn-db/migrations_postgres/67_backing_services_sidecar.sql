-- 67_backing_services_sidecar.sql
--
-- Cut backing services over from the on-demand "worker requests a separate
-- pod + ClusterIP Service at test time" model to declarative native-sidecar
-- injection: a project's selected catalog image declares which services every
-- task-run / verification Pod should provide, and djinn-k8s injects each as a
-- native sidecar (an initContainer with restartPolicy: Always) reachable on
-- 127.0.0.1, with the connection string pre-set in the preset's env var.
--
--   * Drop the provisioning state machine (`service_instances`) — there is no
--     longer any runtime provisioning to track; the sidecar's lifecycle is the
--     Pod's lifecycle.
--   * Drop the per-project kill-switch (`service_policy`) — the image's
--     declared service set is now the sole control surface.
--   * Rename `image_allowed_presets` -> `image_service_presets`: the junction no
--     longer gates what a worker MAY request; it declares what is ALWAYS
--     injected into the image's Pods.
--   * Drop the vestigial `images.allowed_service_presets` JSONB column — the
--     junction table was always the source of truth; the column was written but
--     never read.
--
-- `service_presets` (the curated catalog of injectable services) is retained
-- unchanged — it remains the source of image/port/env/conn-template per service.

DROP TABLE IF EXISTS service_instances;
DROP TABLE IF EXISTS service_policy;

ALTER TABLE images DROP COLUMN IF EXISTS allowed_service_presets;

ALTER TABLE IF EXISTS image_allowed_presets RENAME TO image_service_presets;
