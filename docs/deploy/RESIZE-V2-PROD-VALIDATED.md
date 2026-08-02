# RESIZE-V2-PROD-VALIDATED

Operator evidence record for proposal `hmi6` Item 5b (retire the one-shot
authority-cutover tooling). Gathered 2026-08-01 by the operator from the live
production VPS. All cluster access was read-only.

---

## 1. Deployment identity

| Field | Value |
| --- | --- |
| Deployment revision | `173` |
| Image | `ghcr.io/djinnos/djinn-server:0.7.34` |
| Image digest | `sha256:c489b4fe3435019d7292dc07f1bb2f56c4c74cd2ccd842ea64003ec4dcba163e` |
| Server pod | `djinn-server-5b4cfbb88b-bgpq2`, started `2026-08-01T17:57:09Z`, 0 restarts |
| Cluster | single-node k3s, `vmi3355738`, k8s `v1.35.5+k3s1` |

`pods/resize` RBAC verified live before the observation:

```
Role djinn-controller: resources=['pods/resize'] verbs=['patch']
kubectl auth can-i patch pods/resize --as=system:serviceaccount:djinn:djinn-djinn-controller
  -> yes
```

## 2. Authority mode

The launcher-authority cutover completed earlier the same day:

```
launcher_authority_mode: mode_key=global  mode=resize-v2  epoch=1
  updated_at 2026-08-01 14:26:36.133625+00
images.launcher_authority_protocol = resize-v2
```

Flipped by `deploy/cutover/authority-cutover.sh` through `ResizeRollout::production`,
all ten steps, `pods-created-while-paused=0`:

```
CatalogMutationFrozen, LegacyInventorySigned, ProtocolAwareServerDeployed,
CatalogRebuiltAsResizeV2, RetentionVerified, AdmissionPaused, DrainProven,
PreflightCleared, AuthorityModeFlipped, AdmissionResumed
```

## 3. Observation window

`2026-08-01T17:57:09Z` (server pod start) to `2026-08-01T19:37:43Z` (last sample).
All counts below are from that single pod's continuous log.

## 4. Permit lifecycle — LIFTED

Live sample at `19:37:43Z`, read directly from the apiserver:

| Field | Value |
| --- | --- |
| task_run_id | `019fbecd-5003-7a91-9089-917bf09da547` |
| permit state | **`lifted`** |
| admitted_cpu_millicores | **`4000`** |
| effective_launcher_protocol | `resize-v2` |
| observed_launcher_protocol | `resize-v2` |
| resize_invocation_id | `019fbed1-b27e-73a3-bcda-49d56f80c046` |
| build-Pod name | `djinn-taskrun-019fbecd-5003-7a91-9089-917bf09da547-cgd7t` |
| build-Pod UID | `b0282252-1c3b-42cb-9a3f-f0bb4cc60063` |
| launcher container | `cgroup-launcher` |

Independent `kubectl` read of the same Pod, sampled repeatedly over 25s:

```
spec.initContainers[cgroup-launcher].resources.limits.cpu         = "4"
status.initContainerStatuses[cgroup-launcher].resources.limits.cpu = "4"
```

Ledger and kubelet agree. Note the apiserver canonicalises `4000m` to `4`.

## 5. Permit lifecycle — TERMINAL DROP

Confirmed drops in the observation window:

```
"task_run_resize_drop: returning the launcher to its birth limit"        46
"task_run_resize_drop: birth limit confirmed from init-container status" 36
"task_run_resize_drop: the launcher would not return" (quarantine)        0
```

Drop causes: `normal_exit` (invocation end, via the `release_lease` RPC) and
`server_restart` (the 30s reconciler). Both trigger paths are live.

A complete cycle for the run above:

```
19:29:36.934  task_run_resize_drop: returning the launcher to its birth limit
19:29:38.768  task_run_resize_drop: birth limit confirmed from init-container status
19:30:00.315  task_run_resize_drop: returning the launcher to its birth limit
19:30:04.865  task_run_resize_drop: birth limit confirmed from init-container status
```

This is the many-invocations-per-run cycle that migration
`168_*.sql:5-11` describes — a task run "executes MANY invocations, each of which
crosses the CPU threshold, lifts and drops on that single row".

**Why the confirmation is authoritative.** `settle_confirmed`
(`server/src/task_run_resize_drop.rs:598-616`) logs only after BOTH
`resize_launcher_cpu` returned `Ok` — which internally confirms the new limit by
reading `status.initContainerStatuses[cgroup-launcher]` in millicores, not from
spec and not from a label — AND the `drop_applying -> birth_confirmed` CAS
succeeded. Migration 168's trigger permits `resize_invocation_id` to be written
only on `birth_confirmed -> lift_applying`, and the only edge back into
`birth_confirmed` is `drop_applying -> birth_confirmed`; so a `birth_confirmed`
row carrying a non-null `resize_invocation_id` is itself proof a drop completed.

## 6. Permit lifecycle — RELEASE

```
released permits: 5
release reasons:  resize_owner_gone            2
                  resize_pod_uid_absent        2
                  resize_quarantine_pod_deleted 1
```

Worked example, full lifecycle:

| Field | Value |
| --- | --- |
| task_run_id | `019fbe3a-0e7e-7fd2-bdec-f358cfe8a80b` |
| permit_id | `db35d55d-19cf-4241-bff6-7e5cd6b00027` |
| Pod / UID | `djinn-taskrun-019fbe3a-…-5j9pd` / `0e99e784-fdb1-4854-88fb-44a1df4473ae` |
| admitted_cpu_millicores | `4000` |
| protocol (effective / observed) | `resize-v2` / `resize-v2` |
| resize_invocation_id | `019fbe55-d6bf-7de2-9352-263253e9b593` |
| acquired_at | `2026-08-01 16:48:23.733934+00` |
| released_at | `2026-08-01 17:21:31.385920+00` |
| release_reason | `resize_quarantine_pod_deleted` |

Note the release reason is the **permit retirement** reason at end-of-run. It is
not a statement about mid-run behaviour: this run lifted and dropped repeatedly
before retiring.

## 7. Independent `kubectl` triple-read — CAPTURED

A high-frequency sampler (poll-on-change, no `watch`) captured the complete
`250m -> 4 -> 250m` cycle directly from the apiserver on a live Pod. This is an
operator observation, independent of djinn's own reader.

Pod `djinn-taskrun-019fbf05-36b0-71e3-a5f2-db8a8697ba34-bpbw7`, 2026-08-01:

```
20:30:48.497   spec=4      status=4       <- lifted
20:30:51.427   spec=250m   status=4       <- drop PATCHed; kubelet still actuating
20:30:53.508   spec=250m   status=250m    <- drop actuated and observable
```

The birth clamp was separately captured on an earlier Pod at 20:04:16.604
(`djinn-taskrun-019fbee7-54b4-7bc3-aabb-a4230ff76bde-xtmmq`, `spec=250m
status=250m`), which also shows a Pod living its whole life at the birth clamp
without ever lifting — the clamp holds rather than everything ratcheting up.

**The middle frame is the important one.** At 20:30:51 `spec` already read
`250m` while `status` still read `4`. Spec is the *request*; only
`status.initContainerStatuses` reflects what the kubelet actually applied. A
validator reading spec would have recorded a drop ~2 seconds before it took
effect. This is why `resize_launcher_cpu` confirms from status in millicores,
and why the sampler above records both columns.

Note the apiserver canonicalises `4000m` to `4`; compare in millicores, never as
strings.

Method, for reproduction:

```bash
# poll-on-change; a fixed 2s `watch` misses the 1-2s cycle entirely
POD=<taskrun-pod>
prev=""
while :; do
  S=$(kubectl -n djinn get pod $POD -o jsonpath='{range .spec.initContainers[?(@.name=="cgroup-launcher")]}{.resources.limits.cpu}{end}')
  V=$(kubectl -n djinn get pod $POD -o jsonpath='{range .status.initContainerStatuses[?(@.name=="cgroup-launcher")]}{.resources.limits.cpu}{end}')
  [ "$S|$V" != "$prev" ] && { echo "$(date -u +%H:%M:%S.%3N) spec=$S status=$V"; prev="$S|$V"; }
done
```

## 8. Two defects found during validation — BOTH FIXED, MERGED, DEPLOYED, VERIFIED

Both were measured on the live VPS, fixed, and re-verified in production on
`v0.7.35`. See section 10 for the post-fix verification.

1. **A refused closing lift strands the Pod at 4 cores.** The 4000m PATCH lands
   and is status-confirmed, then the `LiftApplying -> Lifted` CAS is refused, and
   the fallback `require_drop` only logs — it never PATCHes. Measured: permit row
   `state=birth_confirmed admitted=4000` while the live Pod read `"4"` in spec and
   status. `strandedness()` calls `birth_confirmed` + live owner `Live`, so the
   reconciler will not correct it. 11 occurrences in the window (~26% of lifts);
   the Pod holds 4 unleased cores until the run goes terminal.
   **Fix: PR #2893** (`fix/resize-refused-closing-lift-returns-to-birth`) — merged as
   `97f1ff0fc60050bb68857769946f49e240af0f41` and deployed in `v0.7.35`. Reproduced against real Postgres: with the fix removed the
   launcher status reads the ceiling while the permit row reads
   `birth_confirmed`. No migration.

2. **The 30s reconciler steals in-flight drops.** `strandedness()` classifies
   `DropApplying` as `DropOwed` unconditionally on the premise that "no live
   driver rests in these states", but a live worker transits it for ~1s. 6 of 12
   resumptions raced a live worker; 3 produced `LeaseUnavailable` for a drop that
   had in fact succeeded, and three consecutive of those force a cancel.
   **Fix: PR #2894** (`fix/resize-reconciler-steals-inflight-drop`) — merged as
   `ba69b19b8e5be32e98c49ff164f012e23e2ea445` and deployed in `v0.7.35`. Adds
   `state_changed_at` (migration **170**) stamped by the existing
   trigger, and graces `DropApplying`/`DropRequired` for
   `2 x DROP_GATE_BUDGET` = 90s.

   **Final migration order:** #2894 merged first with migration 170. HMI6 PR
   #2889 then merged as `98b18ddeb20b8b7ce176ca8cf52e8311f3e658b0`
   with migration 171. The sequence on `main` is contiguous.

## 9. Note on the automated proof

`server/tests/task_run_resize_cycles.rs` contains a 40-cycle lift+drop live test
with a `250m` readback from `initContainerStatuses`. The live test has **not yet completed successfully**: it is `#[ignore]` during
ordinary test runs and gated behind `DJINN_TEST_RESIZE_CYCLES=1`. Workflow run
`30720369261` was dispatched, but cluster setup failed before the live step
because Helm collided with the pre-existing `djinn` namespace. PR #2896 fixed
that harness ownership defect on `main`; a replacement run is supplemental
confidence evidence, not a prerequisite for the production-backed retirement
record.

The production evidence in this record is therefore stronger than the automated
proof, not a substitute for it.


## 10. Post-fix verification on v0.7.35

Deployment after the fixes:

| Field | Value |
| --- | --- |
| Image | `ghcr.io/djinnos/djinn-server:0.7.35` |
| Image digest | `sha256:df180f82a3822283918bd5f81657858180702e6abff8f619308ea05a80dc1c3c` |
| Release commit | `ba69b19b8e5be32e98c49ff164f012e23e2ea445` |
| Server pod | `djinn-server-57ddd4c479-kbb6r`, ready, 0 restarts |
| Migration | `170 build pod permit state changed at` — applied, `success=t` |
| Trigger | `build_pod_permits_immutable_trigger`, `tgenabled=O` |

### The trigger stamps per-row, so the grace is not inert

Migration 170 backfilled every existing row with a single `now()`
(`21:45:17.665708`, 39 rows). Rows that have changed state since carry their own
distinct timestamps:

```
lifted       2026-08-01 21:48:33.445077+00
released     2026-08-01 21:47:53.543428+00
released     2026-08-01 21:45:21.174947+00
job_created  2026-08-01 21:45:17.665708+00   <- backfill
```

This check is load-bearing. #2894's own mutation testing showed that a
`state_changed_at` set only at INSERT leaves every test green while granting no
grace at all in production, where every real permit row is hours old — the fix
would ship green and completely inert. Distinct, advancing timestamps are the
proof it is live.

### Neither defect recurs, and legitimate reconciliation still works

Counters on the `v0.7.35` pod under real traffic:

```
liftrefused   = 0   ("could not mark the permit drop-required")
leaseunavail  = 0   ("not back at its birth limit")
panic         = 0
strandedresume = 2  (reconciler still reclaiming genuine strands)
```

The two resumptions were `state=Lifted` with `cause=server_restart` — rows left
lifted by a worker the deploy killed. That is a genuine strand and reclaiming it
is precisely what the reconciler exists for; #2894 was never meant to stop that,
only to stop it stealing drops from LIVE workers.

Worker drops in the same window completed untouched:

```
21:47:48.482  drop: returning the launcher to its birth limit   cause=normal_exit
21:47:51.512  drop: birth limit confirmed from init-container status
21:48:17.663  drop: returning the launcher to its birth limit   cause=normal_exit
21:48:18.948  drop: birth limit confirmed from init-container status
```

Before the fix, a 30s sweep landing inside those 1-3 second windows would have
resumed them and returned `LeaseUnavailable` for a drop that had succeeded —
three consecutive of which force a task-run cancel. Zero occurrences.

## 11. Item 5b retirement decision and operator sign-off

The operator accepts the live production evidence in this record as the
retirement gate for HMI6 Item 5b. The independent apiserver triple-read, durable
permit lifecycle, post-fix `v0.7.35` observation, and absence of either measured
defect are sufficient to retire the one-shot authority-cutover path.

The 40-cycle workflow is supplemental confidence evidence. Run `30720369261`
failed during disposable-cluster setup before the live proof; PR #2896 corrected
the namespace owner collision. Its result does not override the direct
production observations recorded above and is not a condition of this sign-off.

**Operator decision (2026-08-01): approved for retirement.** The one-shot
`authority-cutover` wrapper, binary, composition module, and its dedicated test
may be removed. Permanent fail-closed validation, the durable admin runbook, durable
launcher-authority administration, and resize-v2 runtime reconciliation remain.
