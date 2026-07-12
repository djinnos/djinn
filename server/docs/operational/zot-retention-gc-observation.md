# Zot Retention and GC Post-Enable Observation Guidance

This document identifies the post-enable observations required to confirm
Zot catalog image tag reclamation after destructive retention is enabled.
**Production execution of these observations is explicitly operator-owned** —
they are not automated by the Djinn server. The server's startup preflight
produces a deterministic dry-run report (see
`djinn-image-controller::retention_preflight::run_preflight`) that operators
should review *before* enabling destructive mode and consult again *after*
enablement to verify expected reclamation.

## Observability surface

The Zot retention preflight report is the documented observability surface for
the `zot_retention` row of the cross-path cache-cleanup observability matrix.
It is **not** a Prometheus metric — the coordinator-owned
`djinn_cache_cleanup_total` metric model covers `sccache`,
`cargo_target_runs`, and `cargo_warm_base` components; Zot retention is an
externally executed registry action whose bounded state is represented in the
deterministic report, not in coordinator metrics.

## Bounded mode/outcome representation

The preflight report always begins with a bounded mode and outcome header:

| Mode         | Outcome              | When                                                   |
|--------------|----------------------|--------------------------------------------------------|
| `disabled`   | `disabled`           | Retention not enabled — no Zot contact, no plan.       |
| `dry_run`    | `advisory`           | Dry-run report produced; never blocks.                 |
| `destructive`| `destructive_safe`   | Destructive enabled and all selected images pullable.  |
| `destructive`| `destructive_blocked`| Destructive enabled but ≥1 selected image unsafe.      |
| (any)        | `fetch_error`        | Zot state fetch or DB enumeration failed — fail-closed.|

## Report fields

The report exposes:

- **Candidate tags**: total tags across all `djinn-image-*` repos.
- **Retained tags**: tags surviving the newest-N policy.
- **Deleted tags**: tags slated for deletion.
- **Projected reclaimed bytes**: sum of deleted tag sizes.
- **Projected retained bytes**: sum of retained tag sizes.
- **Selected images safe / unsafe**: count of selected catalog images proven
  pullable vs. blocked.
- **Per-repo detail**: retained/deleted tag names, digests, and sizes.
- **Selected-image pins**: tag-retained, digest-retained, or alias-retained
  safety reasons.

## Post-enable observation checklist

After enabling destructive retention (`DJINN_ZOT_RETENTION_ENABLED=true`,
`DJINN_ZOT_RETENTION_DRY_RUN=false`), an operator should:

1. **Confirm preflight passed**: check the server startup log for
   `mode=destructive outcome=destructive_safe`. If the outcome is
   `destructive_blocked`, the server refuses to start — review the report's
   UNSAFE section and fix the offending selected images before retrying.

2. **Verify tag count reduction**: query the Zot registry
   (`/v2/<repo>/tags/list`) for each `djinn-image-*` repo and confirm the tag
   count matches the retained set from the preflight report. Deleted tags
   should no longer appear.

3. **Confirm storage reclamation**: compare actual Zot storage usage before
   and after enablement. The projected reclaimed bytes from the report should
   approximate the observed storage reduction. Note: shared layer dedup may
   cause actual reclamation to differ from the projection.

4. **Check Zot GC logs**: verify Zot's garbage collection has run
   successfully after tag deletion. Zot's `gcInterval` setting controls the
   cadence; check the Zot pod logs for GC completion messages.

5. **Verify selected images remain pullable**: for each selected catalog
   image, confirm it can still be pulled by its tag or digest pin. This
   validates the preflight's safety analysis held in production.

6. **Monitor for build failures**: watch for image-build failures in the
   minutes/hours after enablement. If builds fail with "manifest unknown" or
   similar pull errors, a selected image may have been incorrectly deleted —
   roll back by setting `DJINN_ZOT_RETENTION_DRY_RUN=true` or
   `DJINN_ZOT_RETENTION_ENABLED=false`.

## Rollback

To roll back destructive retention:

- Set `DJINN_ZOT_RETENTION_DRY_RUN=true` (advisory mode, no deletion).
- Or set `DJINN_ZOT_RETENTION_ENABLED=false` (fully disabled).

Both cause the server to start without executing destructive retention. The
Zot configmap should also be updated to remove or disable
`storage.retention` to prevent Zot-side GC from running independently.

## Helm/runtime settings

The following environment variables control the preflight (rendered by the
closed `wsvd` epic into `deployment-server.yaml`):

| Variable                              | Default | Description                          |
|---------------------------------------|---------|--------------------------------------|
| `DJINN_ZOT_RETENTION_ENABLED`         | `false` | Master toggle for retention preflight.|
| `DJINN_ZOT_RETENTION_DRY_RUN`         | `true`  | Advisory mode (no deletion) when enabled.|
| `DJINN_ZOT_RETENTION_NEWEST_TAGS`     | `5`     | Newest-N tags to retain per repo.    |
| `DJINN_ZOT_RETENTION_ENDPOINT`        | registry| Zot HTTP API endpoint.               |
| `DJINN_ZOT_RETENTION_USERNAME`        | (none)  | Basic-auth username (optional).      |
| `DJINN_ZOT_RETENTION_PASSWORD`        | (none)  | Basic-auth password (optional).      |

## Related

- Proposal `0nt2`: Bound VPS shared-cache growth.
- Epic `wsvd`: Zot catalog image tag retention and selected-image preflight.
- Epic `mwex`: Shared-cache cleanup rollout runbook and cross-path observability closeout.
- Canonical runbook: `server/docs/operational/shared-cache-cleanup-runbook.md` (sibling task `bxj7`).
- Confirmation checklist: sibling task `9y26`.
